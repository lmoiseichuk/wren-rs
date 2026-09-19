//! The core library: the methods built-in types respond to.
//!
//! **Everything in Wren is a method call**, including arithmetic — `1 + 2` is
//! `1.+(2)`, dispatched on `Num` like any other message. That uniformity is why
//! the interpreter loop has one call path and no special cases for operators,
//! and it is why this file exists: the operators have to live somewhere.
//!
//! Upstream writes much of its core library *in Wren*, loaded from
//! `wren_core.wren` at start-up, with only the leaves as C primitives. That
//! costs a compile of several hundred lines on every boot — part of the 33 KB
//! of compiler stack measured on the C6, and unaffordable on an 8 KB part. Here
//! everything is a primitive. The cost is that some of it is more verbose than
//! the Wren version would be; the benefit is that starting the VM compiles
//! nothing at all.
//!
//! Error messages are upstream's, word for word, because the test suite checks
//! them.
//!
//! # The calling convention, and why it is an index
//!
//! A primitive receives `&mut Vm` and the **stack index of the receiver**, not
//! a slice of arguments. The receiver is at `at`, the first argument at
//! `at + 1`, and so on — which is why the helpers here are named `receiver` and
//! `argument(_, 1)`, matching upstream's `args[0]` and `args[1]`.
//!
//! A slice would read better and does not typecheck: the arguments live in
//! `vm.stack`, so borrowing them as a slice borrows the VM immutably for the
//! whole call, while every interesting primitive needs it mutably to allocate.
//! The alternative — copying arguments into a fixed buffer on entry — costs up
//! to 17 `Value`s of memcpy on the hottest path in the language, since every
//! `a + b` goes through here. An index costs nothing and sidesteps both.
//!
//! The price is that a primitive can read past its own arguments if it asks for
//! the wrong index. That is a bug of the same shape as reading `args[2]` of a
//! one-argument method in C, and it is caught the same way: by the signature
//! and the tests, not by the type system.

extern crate alloc;

use alloc::string::ToString;

use alloc::boxed::Box;

use alloc::vec::Vec;

use crate::handle::ObjectId;
use crate::math;
use crate::object::{
    MapEntry, ObjClass, ObjFiber, ObjInstance, ObjList, ObjMap, ObjRange, ObjString, Object,
    ObjectType, Primitive,
};
use crate::value::{Num, Value};
use crate::vm::{RuntimeError, Switch, Vm};

/// Bind a primitive to a signature on a class.
fn define(vm: &mut Vm, class: ObjectId, signature: &str, function: Primitive) {
    let symbol = vm.method_names.ensure(signature);
    vm.bind_primitive(class, symbol, function);
}

/// The receiver of the call in progress.
fn receiver(vm: &Vm, at: usize) -> Value {
    vm.stack[at]
}

/// Argument `index`, counting from one as Wren's own `args[1]` does.
fn argument(vm: &Vm, at: usize, index: usize) -> Value {
    vm.stack[at + index]
}

/// An argument that must be a whole number, with upstream's wording.
///
/// **Wren distinguishes "not a number" from "not an integer"** and the suite
/// checks both messages, so this cannot collapse into one check. `noun` is the
/// word the message starts with -- "Index", "Iterator", "Count".
fn integer_argument(vm: &Vm, at: usize, index: usize, noun: &str) -> Result<Num, RuntimeError> {
    let value = argument(vm, at, index)
        .as_num()
        .ok_or_else(|| RuntimeError::new(alloc::format!("{noun} must be a number.")))?;
    if value != math::trunc(value) {
        return Err(RuntimeError::new(alloc::format!(
            "{noun} must be an integer."
        )));
    }
    Ok(value)
}

/// Can this value be a map key?
///
/// **Only the value types**: booleans, classes, null, numbers, ranges and
/// strings. A list or an instance is excluded because Wren hashes a key by its
/// contents, and a mutable object's contents can change after it is inserted --
/// which would lose the entry rather than fail loudly.
fn is_value_type(vm: &Vm, value: Value) -> bool {
    if value.is_num() || value.is_bool() || value.is_null() {
        return true;
    }
    // Three types, so this asks the handle what it is rather than fetching
    // the object to look at its discriminant.
    matches!(
        value.as_object().and_then(|id| vm.heap.type_of(id)),
        Some(ObjectType::String | ObjectType::Range | ObjectType::Class)
    )
}

fn value_type_argument(vm: &Vm, at: usize, index: usize) -> Result<Value, RuntimeError> {
    let value = argument(vm, at, index);
    if is_value_type(vm, value) {
        return Ok(value);
    }
    Err(RuntimeError::new("Key must be a value type."))
}

/// Check that a function takes exactly the arguments a core method will pass.
fn expect_arity(vm: &Vm, function: Value, wanted: usize) -> Result<(), RuntimeError> {
    let Some(closure) = function.as_object() else {
        return Err(RuntimeError::new("Argument must be a function."));
    };
    let Some(arity) = vm.arity_of(closure) else {
        return Err(RuntimeError::new("Argument must be a function."));
    };
    if arity > wanted {
        return Err(RuntimeError::new("Function expects more arguments."));
    }
    Ok(())
}

fn number_argument(vm: &Vm, at: usize, index: usize) -> Result<Num, RuntimeError> {
    argument(vm, at, index)
        .as_num()
        .ok_or_else(|| RuntimeError::new("Right operand must be a number."))
}

/// Define a binary arithmetic operator on `Num`.
macro_rules! arithmetic {
    ($vm:expr, $class:expr, $signature:literal, $left:ident, $right:ident, $body:expr) => {
        define($vm, $class, $signature, |vm, at| {
            let $left = receiver(vm, at).as_num().unwrap_or(Num::NAN);
            let $right = number_argument(vm, at, 1)?;
            Ok($body)
        });
    };
}

/// Install the core library into a fresh VM.
pub fn install(vm: &mut Vm) {
    install_object(vm);
    install_class(vm);
    install_fn(vm);
    install_fiber(vm);
    install_num(vm);
    install_bool(vm);
    install_null(vm);
    install_string(vm);
    install_string_extras(vm);
    install_string_views(vm);
    install_num_extras(vm);
    install_sequence(vm);
    install_list(vm);
    install_list_extras(vm);
    install_map(vm);
    install_range(vm);
    install_system(vm);
    // Last, so that a class which made its own metaclass keeps it.
    install_metaclasses(vm);
}

/// `Object`: what every value responds to, because every class inherits it.
fn install_object(vm: &mut Vm) {
    let class = vm.object_class;

    // **`!anything` is `false`.** Only `Bool` and `Null` override this, which
    // is exactly why `!0` and `!""` are `false` in Wren where most languages
    // would say `true`.
    define(vm, class, "!", |_, _| Ok(Value::FALSE));

    // Identity, which the types with a richer notion of equality override:
    // `Num` compares numerically and `String` compares contents.
    define(vm, class, "==(_)", |vm, at| {
        Ok(Value::bool(receiver(vm, at).is_same(argument(vm, at, 1))))
    });
    define(vm, class, "!=(_)", |vm, at| {
        Ok(Value::bool(!receiver(vm, at).is_same(argument(vm, at, 1))))
    });

    define(vm, class, "toString", |vm, at| {
        let text = vm.to_string(receiver(vm, at));
        Ok(vm.new_string(&text))
    });

    // `a is B` walks up from `a`'s class looking for `B`, so it answers true
    // for a superclass as well as for the exact class.
    define(vm, class, "is(_)", |vm, at| {
        let Some(wanted) = argument(vm, at, 1).as_object() else {
            return Err(RuntimeError::new("Right operand must be a class."));
        };
        if !matches!(vm.heap.get(wanted), Some(Object::Class(_))) {
            return Err(RuntimeError::new("Right operand must be a class."));
        }
        let mut current = vm.class_of(receiver(vm, at));
        while let Some(class) = current {
            if class == wanted {
                return Ok(Value::TRUE);
            }
            current = match vm.heap.class(class) {
                Some(class) => class.superclass,
                _ => None,
            };
        }
        Ok(Value::FALSE)
    });

    // `Object.same(a, b)` is identity, ignoring any `==` a class defines --
    // which is the point of having it.
    let metaclass_name = vm
        .heap
        .allocate(Object::String(ObjString::from_text("Object metaclass")));
    let object_metaclass = vm.heap.allocate(Object::Class(Box::new(ObjClass::new(
        metaclass_name,
        Some(vm.class_class),
    ))));
    if let Some(object) = vm.heap.class_mut(class) {
        object.metaclass = Some(object_metaclass);
    }
    define(vm, object_metaclass, "same(_,_)", |vm, at| {
        // **Value types compare by value**, so `Object.same(1..2, 1..2)` is
        // true even though they are two objects. What `same` ignores is any
        // `==` a *class* defines -- not the identity of the built-in values.
        Ok(Value::bool(values_equal(
            vm,
            argument(vm, at, 1),
            argument(vm, at, 2),
        )))
    });

    define(vm, class, "type", |vm, at| {
        match vm.class_of(receiver(vm, at)) {
            Some(class) => Ok(Value::object(class)),
            None => Ok(Value::NULL),
        }
    });
}

/// `Fn`: what a function literal is an instance of.
/// `Class`: what every class, and every metaclass, responds to.
fn install_class(vm: &mut Vm) {
    let class = vm.class_class;

    define(vm, class, "name", |vm, at| {
        let Some(id) = receiver(vm, at).as_object() else {
            return Err(RuntimeError::new("Receiver must be a class."));
        };
        let name = match vm.heap.class(id) {
            Some(class) => class.name,
            _ => return Err(RuntimeError::new("Receiver must be a class.")),
        };
        Ok(Value::object(name))
    });

    define(vm, class, "supertype", |vm, at| {
        let Some(id) = receiver(vm, at).as_object() else {
            return Err(RuntimeError::new("Receiver must be a class."));
        };
        match vm.heap.class(id) {
            // `Object` has no supertype, which is what makes it the root.
            Some(class) => Ok(class.superclass.map_or(Value::NULL, Value::object)),
            _ => Err(RuntimeError::new("Receiver must be a class.")),
        }
    });

    define(vm, class, "toString", |vm, at| {
        let text = vm.to_string(receiver(vm, at));
        Ok(vm.new_string(&text))
    });

    define(vm, class, "attributes", |vm, at| {
        let Some(id) = receiver(vm, at).as_object() else {
            return Err(RuntimeError::new("Receiver must be a class."));
        };
        match vm.heap.class(id) {
            Some(class) => Ok(class.attributes),
            _ => Err(RuntimeError::new("Receiver must be a class.")),
        }
    });

    // `ClassAttributes` is the pair a class's attributes come back as: the
    // class's own, and its methods'. Two fields and two getters; upstream
    // writes the same class in Wren.
    let attributes_class = vm.class_attributes_class;
    define(vm, attributes_class, "self", |vm, at| {
        Ok(instance_field(vm, receiver(vm, at), 0))
    });
    define(vm, attributes_class, "methods", |vm, at| {
        Ok(instance_field(vm, receiver(vm, at), 1))
    });
}

fn install_fn(vm: &mut Vm) {
    let class = vm.fn_class;

    let metaclass_name = vm
        .heap
        .allocate(Object::String(ObjString::from_text("Fn metaclass")));
    let metaclass = vm.heap.allocate(Object::Class(Box::new(ObjClass::new(
        metaclass_name,
        Some(vm.class_class),
    ))));
    if let Some(function) = vm.heap.class_mut(class) {
        function.metaclass = Some(metaclass);
    }

    // `Fn.new { ... }` -- the block is already a function, so this hands it
    // back. It exists because that is how a function literal is written in
    // Wren: there is no bare block expression, only a block argument.
    define(vm, metaclass, "new(_)", |vm, at| {
        let block = argument(vm, at, 1);
        match block.as_object().and_then(|id| vm.heap.closure(id)) {
            Some(_) => Ok(block),
            None => Err(RuntimeError::new("Argument must be a function.")),
        }
    });

    define(vm, class, "arity", |vm, at| {
        let Some(id) = receiver(vm, at).as_object() else {
            return Err(RuntimeError::new("Receiver must be a function."));
        };
        let arity = vm.arity_of(id).unwrap_or(0);
        Ok(Value::num(arity as Num))
    });

    // **One `call` per arity, because a Wren signature includes its arity.**
    // `call()` and `call(1)` are different methods, not an overload, so each
    // needs its own entry in the table.
    for arity in 0..=16usize {
        let name = signature_for("call", arity);
        define(vm, class, &name, move |vm, at| {
            let Some(closure) = receiver(vm, at).as_object() else {
                return Err(RuntimeError::new("Receiver must be a function."));
            };
            vm.call_closure(closure, at)
        });
    }
}

/// `call`, `call(_)`, `call(_,_)`, ...
fn signature_for(name: &str, arity: usize) -> alloc::string::String {
    // Parenthesised even at arity zero: `call` and `call()` are different
    // methods in Wren.
    let mut out = alloc::string::String::from(name);
    out.push('(');
    for index in 0..arity {
        if index > 0 {
            out.push(',');
        }
        out.push('_');
    }
    out.push(')');
    out
}

/// `Fiber`: coroutines, and the error handling built on them.
///
/// **Wren has no `try`/`catch`.** A runtime error aborts the fiber it happened
/// in, and `fiber.try()` runs one and hands back the error rather than letting
/// it propagate. So this class is both chapters at once.
fn install_fiber(vm: &mut Vm) {
    let class = vm.fiber_class;

    let metaclass_name = vm
        .heap
        .allocate(Object::String(ObjString::from_text("Fiber metaclass")));
    let metaclass = vm.heap.allocate(Object::Class(Box::new(ObjClass::new(
        metaclass_name,
        Some(vm.class_class),
    ))));
    if let Some(fiber) = vm.heap.class_mut(class) {
        fiber.metaclass = Some(metaclass);
    }

    define(vm, metaclass, "new(_)", |vm, at| {
        let function = argument(vm, at, 1);
        let Some(closure) = function.as_object() else {
            return Err(RuntimeError::new("Argument must be a function."));
        };
        if !vm.heap.closure(closure).is_some() {
            return Err(RuntimeError::new("Argument must be a function."));
        }
        // A fiber's function receives at most the one value it was resumed
        // with, so anything taking more could never be called.
        if vm.arity_of(closure).unwrap_or(0) > 1 {
            return Err(RuntimeError::new(
                "Function cannot take more than one parameter.",
            ));
        }
        let id = vm
            .heap
            .allocate(Object::Fiber(Box::new(ObjFiber::new(closure))));
        Ok(Value::object(id))
    });

    define(vm, metaclass, "current", |vm, _| match vm.current_fiber {
        Some(id) => Ok(Value::object(id)),
        None => Ok(Value::NULL),
    });

    // `Fiber.abort(message)` raises a runtime error the way a failing
    // primitive does, so `try` catches it like any other.
    define(vm, metaclass, "abort(_)", |vm, at| {
        let message = argument(vm, at, 1);
        if message.is_null() {
            // Upstream treats aborting with null as "do not actually abort".
            return Ok(Value::NULL);
        }
        let text = vm.to_string(message);
        Err(RuntimeError::new(text))
    });

    define(vm, metaclass, "yield()", |vm, _| {
        yield_to_caller(vm, Value::NULL)
    });
    define(vm, metaclass, "yield(_)", |vm, at| {
        let value = argument(vm, at, 1);
        yield_to_caller(vm, value)
    });

    define(vm, class, "call()", |vm, at| {
        switch_into(vm, at, Value::NULL, true, false)
    });
    define(vm, class, "call(_)", |vm, at| {
        let value = argument(vm, at, 1);
        switch_into(vm, at, value, true, false)
    });
    define(vm, class, "try()", |vm, at| {
        switch_into(vm, at, Value::NULL, true, true)
    });
    define(vm, class, "try(_)", |vm, at| {
        let value = argument(vm, at, 1);
        switch_into(vm, at, value, true, true)
    });
    // `transfer` does not record a caller, so the fiber it leaves is not
    // resumed when the target finishes -- a jump rather than a call.
    define(vm, class, "transfer()", |vm, at| {
        switch_into(vm, at, Value::NULL, false, false)
    });
    define(vm, class, "transfer(_)", |vm, at| {
        let value = argument(vm, at, 1);
        switch_into(vm, at, value, false, false)
    });
    // Transfer control to a fiber *and* make it fail there, so whoever is
    // running it with `try` sees the error as if it had raised one itself.
    define(vm, class, "transferError(_)", |vm, at| {
        let value = argument(vm, at, 1);
        switch_into(vm, at, value, false, false)?;
        if let Some(switch) = vm.pending_switch.as_mut() {
            switch.as_error = true;
        }
        Ok(Value::NULL)
    });

    define(vm, class, "isDone", |vm, at| {
        match receiver(vm, at)
            .as_object()
            .and_then(|id| vm.heap.fiber(id))
        {
            Some(fiber) => Ok(Value::bool(fiber.done)),
            _ => Err(RuntimeError::new("Receiver must be a fiber.")),
        }
    });

    define(vm, class, "error", |vm, at| {
        match receiver(vm, at)
            .as_object()
            .and_then(|id| vm.heap.fiber(id))
        {
            Some(fiber) => Ok(fiber.error),
            _ => Err(RuntimeError::new("Receiver must be a fiber.")),
        }
    });
}

/// Ask the interpreter to continue in `fiber`.
fn switch_into(
    vm: &mut Vm,
    at: usize,
    value: Value,
    set_caller: bool,
    catching: bool,
) -> Result<Value, RuntimeError> {
    let Some(target) = receiver(vm, at).as_object() else {
        return Err(RuntimeError::new("Receiver must be a fiber."));
    };
    match vm.heap.fiber(target) {
        Some(fiber) if fiber.done => {
            return Err(RuntimeError::new("Cannot call a finished fiber."));
        }
        // **`call` and `try` only.** The root fiber is the one doing the
        // calling, so resuming it that way would re-enter a live stack. But
        // `transfer` to it is exactly how a fiber hands control back for good,
        // and refusing that broke four tests that were right.
        Some(_) if set_caller && vm.root_fiber == Some(target) => {
            return Err(RuntimeError::new("Cannot call root fiber."));
        }
        Some(_) => {}
        _ => return Err(RuntimeError::new("Receiver must be a fiber.")),
    }
    vm.pending_switch = Some(Switch {
        target,
        value,
        set_caller,
        catching,
        finishing: false,
        as_error: false,
    });
    Ok(Value::NULL)
}

/// Suspend the running fiber and hand `value` back to whoever resumed it.
fn yield_to_caller(vm: &mut Vm, value: Value) -> Result<Value, RuntimeError> {
    let caller = vm.current_fiber.and_then(|id| match vm.heap.fiber(id) {
        Some(fiber) => fiber.caller,
        _ => None,
    });
    let Some(caller) = caller else {
        // **Yielding from the root fiber stops the program.** There is nobody
        // to hand control back to, and upstream treats that as the end of the
        // run rather than as an error -- `yield_from_main` prints what came
        // before the yield and nothing after it.
        vm.halting = true;
        return Ok(Value::NULL);
    };
    vm.pending_switch = Some(Switch {
        target: caller,
        value,
        set_caller: false,
        catching: false,
        finishing: false,
        as_error: false,
    });
    Ok(Value::NULL)
}

fn install_num(vm: &mut Vm) {
    let class = vm.num_class;

    arithmetic!(vm, class, "+(_)", a, b, Value::num(a + b));
    arithmetic!(vm, class, "-(_)", a, b, Value::num(a - b));
    arithmetic!(vm, class, "*(_)", a, b, Value::num(a * b));
    arithmetic!(vm, class, "/(_)", a, b, Value::num(a / b));
    // Wren's `%` follows C's fmod: the result takes the sign of the dividend,
    // which is *not* what Rust's `rem_euclid` does.
    arithmetic!(vm, class, "%(_)", a, b, Value::num(a % b));
    arithmetic!(vm, class, "<(_)", a, b, Value::bool(a < b));
    arithmetic!(vm, class, ">(_)", a, b, Value::bool(a > b));
    arithmetic!(vm, class, "<=(_)", a, b, Value::bool(a <= b));
    arithmetic!(vm, class, ">=(_)", a, b, Value::bool(a >= b));

    // Ranges are built by operators on numbers, not by a constructor.
    //
    // **Written out rather than put through `arithmetic!`.** These allocate,
    // so their bodies need the VM -- and a body passed into the macro is
    // written at the call site, where `vm` means the *outer* VM rather than the
    // closure's parameter. Macro hygiene is right to bind it that way, and the
    // result is a closure that captures, which is not a `fn` pointer.
    define(vm, class, "..(_)", |vm, at| {
        let from = receiver(vm, at).as_num().unwrap_or(Num::NAN);
        let to = number_argument(vm, at, 1)?;
        let id = vm.heap.allocate(Object::Range(ObjRange {
            from,
            to,
            is_inclusive: true,
        }));
        Ok(Value::object(id))
    });
    define(vm, class, "...(_)", |vm, at| {
        let from = receiver(vm, at).as_num().unwrap_or(Num::NAN);
        let to = number_argument(vm, at, 1)?;
        let id = vm.heap.allocate(Object::Range(ObjRange {
            from,
            to,
            is_inclusive: false,
        }));
        Ok(Value::object(id))
    });

    // **Equality is not an arithmetic operator here**, because the right side
    // may be any type at all: `1 == "one"` is `false`, not an error.
    define(vm, class, "==(_)", |vm, at| {
        let left = receiver(vm, at).as_num();
        let right = argument(vm, at, 1).as_num();
        Ok(Value::bool(match (left, right) {
            (Some(a), Some(b)) => a == b,
            _ => false,
        }))
    });
    define(vm, class, "!=(_)", |vm, at| {
        let left = receiver(vm, at).as_num();
        let right = argument(vm, at, 1).as_num();
        Ok(Value::bool(match (left, right) {
            (Some(a), Some(b)) => a != b,
            _ => true,
        }))
    });

    define(vm, class, "-", |vm, at| {
        Ok(Value::num(-receiver(vm, at).as_num().unwrap_or(Num::NAN)))
    });
    define(vm, class, "abs", |vm, at| {
        Ok(Value::num(math::abs(
            receiver(vm, at).as_num().unwrap_or(Num::NAN),
        )))
    });
    define(vm, class, "floor", |vm, at| {
        Ok(Value::num(math::floor(
            receiver(vm, at).as_num().unwrap_or(Num::NAN),
        )))
    });
    define(vm, class, "ceil", |vm, at| {
        Ok(Value::num(math::ceil(
            receiver(vm, at).as_num().unwrap_or(Num::NAN),
        )))
    });
    define(vm, class, "sqrt", |vm, at| {
        Ok(Value::num(math::sqrt(
            receiver(vm, at).as_num().unwrap_or(Num::NAN),
        )))
    });
    define(vm, class, "toString", |vm, at| {
        let text = vm.to_string(receiver(vm, at));
        Ok(vm.new_string(&text))
    });
}

fn install_bool(vm: &mut Vm) {
    let class = vm.bool_class;
    define(vm, class, "!", |vm, at| {
        Ok(Value::bool(receiver(vm, at).is_falsy()))
    });
    define(vm, class, "toString", |vm, at| {
        let text = vm.to_string(receiver(vm, at));
        Ok(vm.new_string(&text))
    });
    define(vm, class, "==(_)", |vm, at| {
        Ok(Value::bool(receiver(vm, at).is_same(argument(vm, at, 1))))
    });
    define(vm, class, "!=(_)", |vm, at| {
        Ok(Value::bool(!receiver(vm, at).is_same(argument(vm, at, 1))))
    });
}

fn install_null(vm: &mut Vm) {
    let class = vm.null_class;
    // `!null` is `true`: null is falsy, and this is the only way a program can
    // ask about it without a comparison.
    define(vm, class, "!", |_, _| Ok(Value::TRUE));
    define(vm, class, "toString", |vm, _| Ok(vm.new_string("null")));
    define(vm, class, "==(_)", |vm, at| {
        Ok(Value::bool(argument(vm, at, 1).is_null()))
    });
    define(vm, class, "!=(_)", |vm, at| {
        Ok(Value::bool(!argument(vm, at, 1).is_null()))
    });
}

fn install_string(vm: &mut Vm) {
    let class = vm.string_class;

    define(vm, class, "+(_)", |vm, at| {
        let left = vm.to_string(receiver(vm, at));
        let right = argument(vm, at, 1);
        // Upstream requires the right operand to be a string too, rather than
        // coercing it. `"a" + 1` is an error in Wren, and quietly making it
        // work here would be a different language.
        if !vm.is_string(right) {
            return Err(RuntimeError::new("Right operand must be a string."));
        }
        let joined = alloc::format!("{}{}", left, vm.to_string(right));
        Ok(vm.new_string(&joined))
    });

    define(vm, class, "count", |vm, at| {
        // **Code points, not bytes.** Upstream's own test says it: "treats a
        // UTF-8 sequence as a single item", and "counts invalid UTF-8 one byte
        // at a time". Both fall out of counting the bytes that are not
        // continuations. The byte count is `.bytes.count`, a different
        // question and a different method.
        let bytes = string_bytes(vm, receiver(vm, at));
        let count = bytes.iter().filter(|byte| !is_continuation(**byte)).count();
        Ok(Value::num(count as Num))
    });

    define(vm, class, "toString", |vm, at| Ok(receiver(vm, at)));

    define(vm, class, "==(_)", |vm, at| {
        Ok(Value::bool(strings_equal(
            vm,
            receiver(vm, at),
            argument(vm, at, 1),
        )))
    });
    define(vm, class, "!=(_)", |vm, at| {
        Ok(Value::bool(!strings_equal(
            vm,
            receiver(vm, at),
            argument(vm, at, 1),
        )))
    });
}

/// Wren compares strings by **contents**, not by identity.
fn strings_equal(vm: &Vm, left: Value, right: Value) -> bool {
    let (Some(left), Some(right)) = (left.as_object(), right.as_object()) else {
        return false;
    };
    match (vm.heap.string(left), vm.heap.string(right)) {
        (Some(a), Some(b)) => {
            // The cached hash is a cheap rejection before comparing bytes.
            a.hash() == b.hash() && a.bytes == b.bytes
        }
        _ => false,
    }
}

/// The rest of `Num`.
fn install_num_extras(vm: &mut Vm) {
    let class = vm.num_class;

    define(vm, class, "min(_)", |vm, at| {
        let a = receiver(vm, at).as_num().unwrap_or(Num::NAN);
        let b = number_argument(vm, at, 1)?;
        Ok(Value::num(if a < b { a } else { b }))
    });
    define(vm, class, "max(_)", |vm, at| {
        let a = receiver(vm, at).as_num().unwrap_or(Num::NAN);
        let b = number_argument(vm, at, 1)?;
        Ok(Value::num(if a > b { a } else { b }))
    });
    define(vm, class, "clamp(_,_)", |vm, at| {
        let value = receiver(vm, at).as_num().unwrap_or(Num::NAN);
        let low = number_argument(vm, at, 1)?;
        let high = number_argument(vm, at, 2)?;
        let clamped = if value < low {
            low
        } else if value > high {
            high
        } else {
            value
        };
        Ok(Value::num(clamped))
    });

    define(vm, class, "truncate", |vm, at| {
        Ok(Value::num(math::trunc(
            receiver(vm, at).as_num().unwrap_or(Num::NAN),
        )))
    });
    define(vm, class, "fraction", |vm, at| {
        let value = receiver(vm, at).as_num().unwrap_or(Num::NAN);
        let fraction = value - math::trunc(value);
        // `(-2).fraction` is `-0`, not `0`. The subtraction gives a positive
        // zero, and Wren prints the sign, so it has to be put back.
        if fraction == 0.0 && value.is_sign_negative() {
            return Ok(Value::num(-0.0));
        }
        Ok(Value::num(fraction))
    });
    define(vm, class, "sign", |vm, at| {
        let value = receiver(vm, at).as_num().unwrap_or(Num::NAN);
        let sign = if value > 0.0 {
            1.0
        } else if value < 0.0 {
            -1.0
        } else {
            // Zero's sign is zero, not one. Wren follows the sign function
            // rather than `copysign`.
            0.0
        };
        Ok(Value::num(sign))
    });
    define(vm, class, "isInteger", |vm, at| {
        let value = receiver(vm, at).as_num().unwrap_or(Num::NAN);
        Ok(Value::bool(
            value.is_finite() && value == math::trunc(value),
        ))
    });
    define(vm, class, "isNan", |vm, at| {
        Ok(Value::bool(
            receiver(vm, at).as_num().unwrap_or(0.0).is_nan(),
        ))
    });
    define(vm, class, "isInfinity", |vm, at| {
        Ok(Value::bool(
            receiver(vm, at).as_num().unwrap_or(0.0).is_infinite(),
        ))
    });

    // **Wren's bitwise operators work on 32-bit unsigned values**, so a double
    // is truncated and wrapped first and the result comes back as a double.
    // Anything else would make `~0` depend on the width of a C int.
    define(vm, class, "&(_)", |vm, at| bitwise(vm, at, |a, b| a & b));
    define(vm, class, "|(_)", |vm, at| bitwise(vm, at, |a, b| a | b));
    define(vm, class, "^(_)", |vm, at| bitwise(vm, at, |a, b| a ^ b));
    define(vm, class, "<<(_)", |vm, at| {
        bitwise(vm, at, |a, b| a.wrapping_shl(b & 31))
    });
    define(vm, class, ">>(_)", |vm, at| {
        bitwise(vm, at, |a, b| a.wrapping_shr(b & 31))
    });
    define(vm, class, "~", |vm, at| {
        let value = receiver(vm, at).as_num().unwrap_or(Num::NAN);
        Ok(Value::num(!(value as i64 as u32) as Num))
    });

    // **Present when there is a libm to get them from**, which is `std` on a
    // host and the optional `libm` feature on a bare-metal target. With
    // neither they are undefined, and `1.sin` reports "Num does not implement
    // 'sin'" -- an answer the caller can see, rather than one from a series
    // that is quietly wrong in the digits Wren prints.
    #[cfg(any(feature = "std", feature = "libm"))]
    {
        use crate::math::real;

        define(vm, class, "round", |vm, at| {
            Ok(Value::num(real::round(
                receiver(vm, at).as_num().unwrap_or(Num::NAN),
            )))
        });
        define(vm, class, "pow(_)", |vm, at| {
            let base = receiver(vm, at).as_num().unwrap_or(Num::NAN);
            let exponent = number_argument(vm, at, 1)?;
            Ok(Value::num(real::pow(base, exponent)))
        });
        define(vm, class, "log", |vm, at| {
            Ok(Value::num(real::ln(
                receiver(vm, at).as_num().unwrap_or(Num::NAN),
            )))
        });
        define(vm, class, "log2", |vm, at| {
            Ok(Value::num(real::log2(
                receiver(vm, at).as_num().unwrap_or(Num::NAN),
            )))
        });
        define(vm, class, "exp", |vm, at| {
            Ok(Value::num(real::exp(
                receiver(vm, at).as_num().unwrap_or(Num::NAN),
            )))
        });
        define(vm, class, "cbrt", |vm, at| {
            Ok(Value::num(real::cbrt(
                receiver(vm, at).as_num().unwrap_or(Num::NAN),
            )))
        });
        define(vm, class, "sin", |vm, at| {
            Ok(Value::num(real::sin(
                receiver(vm, at).as_num().unwrap_or(Num::NAN),
            )))
        });
        define(vm, class, "cos", |vm, at| {
            Ok(Value::num(real::cos(
                receiver(vm, at).as_num().unwrap_or(Num::NAN),
            )))
        });
        define(vm, class, "tan", |vm, at| {
            Ok(Value::num(real::tan(
                receiver(vm, at).as_num().unwrap_or(Num::NAN),
            )))
        });
        define(vm, class, "asin", |vm, at| {
            Ok(Value::num(real::asin(
                receiver(vm, at).as_num().unwrap_or(Num::NAN),
            )))
        });
        define(vm, class, "acos", |vm, at| {
            Ok(Value::num(real::acos(
                receiver(vm, at).as_num().unwrap_or(Num::NAN),
            )))
        });
        define(vm, class, "atan", |vm, at| {
            Ok(Value::num(real::atan(
                receiver(vm, at).as_num().unwrap_or(Num::NAN),
            )))
        });
        define(vm, class, "atan(_)", |vm, at| {
            let y = receiver(vm, at).as_num().unwrap_or(Num::NAN);
            let x = number_argument(vm, at, 1)?;
            Ok(Value::num(real::atan2(y, x)))
        });
    }

    let metaclass_name = vm
        .heap
        .allocate(Object::String(ObjString::from_text("Num metaclass")));
    let metaclass = vm.heap.allocate(Object::Class(Box::new(ObjClass::new(
        metaclass_name,
        Some(vm.class_class),
    ))));
    if let Some(num) = vm.heap.class_mut(class) {
        num.metaclass = Some(metaclass);
    }
    define(vm, metaclass, "fromString(_)", |vm, at| {
        let text = string_argument(vm, at, 1)?;
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Ok(Value::NULL);
        }
        // Hexadecimal is written the same way a literal is.
        let parsed = match trimmed
            .strip_prefix("0x")
            .or_else(|| trimmed.strip_prefix("0X"))
        {
            Some(digits) => u64::from_str_radix(digits, 16)
                .ok()
                .map(|value| value as Num),
            None => trimmed.parse::<Num>().ok(),
        };
        // **Null rather than an error for junk**: the caller asked whether the
        // text is a number, and "no" is an answer. A number too large to
        // represent is different -- the text *is* a number, and quietly
        // answering `infinity` would be a wrong one.
        match parsed {
            Some(value) if value.is_infinite() => {
                Err(RuntimeError::new("Number literal is too large."))
            }
            Some(value) => Ok(Value::num(value)),
            None => Ok(Value::NULL),
        }
    });

    define(vm, metaclass, "pi", |_, _| {
        Ok(Value::num(core::f64::consts::PI as Num))
    });
    define(vm, metaclass, "e", |_, _| {
        Ok(Value::num(core::f64::consts::E as Num))
    });
    define(vm, metaclass, "infinity", |_, _| {
        Ok(Value::num(Num::INFINITY))
    });
    define(vm, metaclass, "nan", |_, _| Ok(Value::num(Num::NAN)));
    define(vm, metaclass, "largest", |_, _| Ok(Value::num(Num::MAX)));
    define(vm, metaclass, "smallest", |_, _| {
        Ok(Value::num(Num::MIN_POSITIVE))
    });
    define(vm, metaclass, "maxSafeInteger", |_, _| {
        Ok(Value::num(9007199254740991.0))
    });
    define(vm, metaclass, "minSafeInteger", |_, _| {
        Ok(Value::num(-9007199254740991.0))
    });
}

fn bitwise(vm: &Vm, at: usize, operation: fn(u32, u32) -> u32) -> Result<Value, RuntimeError> {
    let left = receiver(vm, at).as_num().unwrap_or(Num::NAN);
    let right = argument(vm, at, 1)
        .as_num()
        .ok_or_else(|| RuntimeError::new("Right operand must be a number."))?;
    Ok(Value::num(
        operation(left as i64 as u32, right as i64 as u32) as Num,
    ))
}

/// The rest of `String`.
fn install_string_extras(vm: &mut Vm) {
    let class = vm.string_class;

    define(vm, class, "[_]", |vm, at| {
        let text = string_bytes(vm, receiver(vm, at));
        // A range subscript takes a substring, as it slices a list.
        if let Some(range) = range_of(vm, argument(vm, at, 1)) {
            // **Byte positions in, whole characters out.** Upstream visits
            // each selected byte, decodes a code point there, and emits it
            // only if one starts at that byte -- so a range beginning or
            // ending inside a sequence quietly drops the partial bytes rather
            // than producing half a character. `"søméஃthîng"[2..6]` is "méஃ":
            // five byte positions, two of them mid-sequence and skipped.
            let taken = slice_indices(&range, text.len())?;
            let mut bytes = Vec::new();
            for index in taken {
                if let Some(character) = code_point_at(&text, index) {
                    let mut buffer = [0u8; 4];
                    bytes.extend_from_slice(character.encode_utf8(&mut buffer).as_bytes());
                }
            }
            return Ok(vm.new_string_bytes(bytes));
        }
        let index = number_argument(vm, at, 1)?;
        let index = resolve_index(index, text.len())?;
        // **Indexing is by byte but yields a whole code point.** Upstream does
        // the same: a string is bytes, but `s[0]` of a multi-byte character is
        // that character rather than half of it -- and a byte that starts no
        // valid sequence is returned as itself rather than reinterpreted.
        Ok(vm.new_string_bytes(character_bytes(&text, index)))
    });

    define(vm, class, "contains(_)", |vm, at| {
        let text = string_text(vm, receiver(vm, at));
        let needle = string_argument(vm, at, 1)?;
        Ok(Value::bool(text.contains(&needle)))
    });
    define(vm, class, "startsWith(_)", |vm, at| {
        let text = string_text(vm, receiver(vm, at));
        let needle = string_argument(vm, at, 1)?;
        Ok(Value::bool(text.starts_with(&needle)))
    });
    define(vm, class, "endsWith(_)", |vm, at| {
        let text = string_text(vm, receiver(vm, at));
        let needle = string_argument(vm, at, 1)?;
        Ok(Value::bool(text.ends_with(&needle)))
    });
    define(vm, class, "indexOf(_)", |vm, at| {
        let text = string_text(vm, receiver(vm, at));
        let needle = string_argument(vm, at, 1)?;
        Ok(Value::num(
            text.find(&needle).map_or(-1.0, |index| index as Num),
        ))
    });

    define(vm, class, "isEmpty", |vm, at| {
        Ok(Value::bool(string_bytes(vm, receiver(vm, at)).is_empty()))
    });

    define(vm, class, "indexOf(_,_)", |vm, at| {
        // Byte-wise, so an index landing inside a multi-byte sequence is an
        // ordinary answer rather than a panic.
        let text = string_bytes(vm, receiver(vm, at));
        let needle = string_bytes(vm, string_value_argument(vm, at, 1)?);
        let start = integer_argument(vm, at, 2, "Start")?;
        // Negative counts from the end, as every other index in the language
        // does. Equal to the length is allowed -- searching an empty tail is a
        // sensible question with the answer -1.
        let resolved = if start < 0.0 {
            start + text.len() as Num
        } else {
            start
        };
        // The start must be a position *in* the string, so equal to the
        // length is already past the end.
        if resolved < 0.0 || resolved >= text.len() as Num {
            return Err(RuntimeError::new("Start out of bounds."));
        }
        let start = resolved as usize;
        Ok(Value::num(
            find_bytes(&text[start..], &needle).map_or(-1.0, |index| (index + start) as Num),
        ))
    });

    define(vm, class, "replace(_,_)", |vm, at| {
        let text = string_text(vm, receiver(vm, at));
        let from = string_argument(vm, at, 1)?;
        let to = string_argument(vm, at, 2)?;
        if from.is_empty() {
            return Err(RuntimeError::new("Cannot replace an empty string."));
        }
        let replaced = text.replace(&from, &to);
        Ok(vm.new_string(&replaced))
    });

    define(vm, class, "trim()", |vm, at| {
        let text = string_text(vm, receiver(vm, at));
        let trimmed = text.trim().to_string();
        Ok(vm.new_string(&trimmed))
    });
    define(vm, class, "trimStart()", |vm, at| {
        let text = string_text(vm, receiver(vm, at));
        let trimmed = text.trim_start().to_string();
        Ok(vm.new_string(&trimmed))
    });
    define(vm, class, "trimEnd()", |vm, at| {
        let text = string_text(vm, receiver(vm, at));
        let trimmed = text.trim_end().to_string();
        Ok(vm.new_string(&trimmed))
    });

    define(vm, class, "trim(_)", |vm, at| {
        let text = string_text(vm, receiver(vm, at));
        let set = string_argument(vm, at, 1)?;
        let trimmed = text
            .trim_start_matches(|c| set.contains(c))
            .trim_end_matches(|c| set.contains(c))
            .to_string();
        Ok(vm.new_string(&trimmed))
    });
    define(vm, class, "trimStart(_)", |vm, at| {
        let text = string_text(vm, receiver(vm, at));
        let set = string_argument(vm, at, 1)?;
        let trimmed = text.trim_start_matches(|c| set.contains(c)).to_string();
        Ok(vm.new_string(&trimmed))
    });
    define(vm, class, "trimEnd(_)", |vm, at| {
        let text = string_text(vm, receiver(vm, at));
        let set = string_argument(vm, at, 1)?;
        let trimmed = text.trim_end_matches(|c| set.contains(c)).to_string();
        Ok(vm.new_string(&trimmed))
    });

    define(vm, class, "split(_)", |vm, at| {
        let text = string_text(vm, receiver(vm, at));
        let separator = string_text(vm, argument(vm, at, 1));
        if separator.is_empty() {
            return Err(RuntimeError::new("Separator cannot be empty."));
        }
        let parts: Vec<alloc::string::String> = text
            .split(&separator)
            .map(|part| part.to_string())
            .collect();
        let values: Vec<Value> = parts.iter().map(|part| vm.new_string(part)).collect();
        Ok(new_list(vm, values))
    });

    define(vm, class, "*(_)", |vm, at| {
        let text = string_text(vm, receiver(vm, at));
        let count = number_argument(vm, at, 1)?;
        if count < 0.0 || count != math::trunc(count) {
            return Err(RuntimeError::new("Count must be a non-negative integer."));
        }
        let repeated = text.repeat(count as usize);
        Ok(vm.new_string(&repeated))
    });

    // The byte and code-point views. Upstream returns lazy sequences; these are
    // lists, which answers `count`, `[_]` and iteration the same way.
    // **Views over the string, not lists copied out of it.** A program can
    // ask `s.bytes is StringByteSequence`, and building a list would also
    // snapshot a string rather than read it.
    define(vm, class, "bytes", |vm, at| {
        let class = vm.string_byte_sequence_class;
        Ok(new_view(vm, class, &[receiver(vm, at)]))
    });
    define(vm, class, "codePoints", |vm, at| {
        let class = vm.string_code_point_sequence_class;
        Ok(new_view(vm, class, &[receiver(vm, at)]))
    });

    // Iterating a string yields its characters, one code point at a time.
    define(vm, class, "iterate(_)", |vm, at| {
        let bytes = string_bytes(vm, receiver(vm, at));
        let current = argument(vm, at, 1);
        let start = if current.is_null() {
            0
        } else {
            let index = integer_argument(vm, at, 1, "Iterator")?;
            if index < 0.0 || index as usize >= bytes.len() {
                return Ok(Value::FALSE);
            }
            // One byte on, then past any continuation bytes. This always
            // advances, so a malformed string still terminates.
            index as usize + 1
        };
        let mut start = start;
        while start < bytes.len() && is_continuation(bytes[start]) {
            start += 1;
        }
        if start >= bytes.len() {
            return Ok(Value::FALSE);
        }
        Ok(Value::num(start as Num))
    });
    define(vm, class, "iteratorValue(_)", |vm, at| {
        let bytes = string_bytes(vm, receiver(vm, at));
        let index = integer_argument(vm, at, 1, "Iterator")?;
        if index < 0.0 || index as usize >= bytes.len() {
            return Err(RuntimeError::new("Iterator out of bounds."));
        }
        Ok(vm.new_string_bytes(character_bytes(&bytes, index as usize)))
    });

    define(vm, class, "<(_)", |vm, at| {
        compare_strings(vm, at, |o| o < 0)
    });
    define(vm, class, ">(_)", |vm, at| {
        compare_strings(vm, at, |o| o > 0)
    });
    define(vm, class, "<=(_)", |vm, at| {
        compare_strings(vm, at, |o| o <= 0)
    });
    define(vm, class, ">=(_)", |vm, at| {
        compare_strings(vm, at, |o| o >= 0)
    });

    let metaclass_name = vm
        .heap
        .allocate(Object::String(ObjString::from_text("String metaclass")));
    let metaclass = vm.heap.allocate(Object::Class(Box::new(ObjClass::new(
        metaclass_name,
        Some(vm.class_class),
    ))));
    if let Some(string) = vm.heap.class_mut(class) {
        string.metaclass = Some(metaclass);
    }
    define(vm, metaclass, "fromCodePoint(_)", |vm, at| {
        let point = integer_argument(vm, at, 1, "Code point")?;
        if point < 0.0 {
            return Err(RuntimeError::new("Code point cannot be negative."));
        }
        if point > 0x10ffff as Num {
            return Err(RuntimeError::new(
                "Code point cannot be greater than 0x10ffff.",
            ));
        }
        let Some(character) = char::from_u32(point as u32) else {
            return Err(RuntimeError::new(
                "Code point cannot be greater than 0x10ffff.",
            ));
        };
        let text = alloc::string::String::from(character);
        Ok(vm.new_string(&text))
    });
    define(vm, metaclass, "fromByte(_)", |vm, at| {
        let byte = number_argument(vm, at, 1)?;
        if !(0.0..=255.0).contains(&byte) || byte != math::trunc(byte) {
            return Err(RuntimeError::new(
                "Byte must be an integer between 0 and 255.",
            ));
        }
        let id = vm
            .heap
            .allocate(Object::String(ObjString::new(alloc::vec![byte as u8])));
        Ok(Value::object(id))
    });
}

fn compare_strings(vm: &Vm, at: usize, accept: fn(i32) -> bool) -> Result<Value, RuntimeError> {
    let left = string_bytes(vm, receiver(vm, at));
    let right = match argument(vm, at, 1)
        .as_object()
        .and_then(|id| vm.heap.string(id))
    {
        Some(text) => text.bytes.clone(),
        _ => return Err(RuntimeError::new("Right operand must be a string.")),
    };
    let ordering = match left.cmp(&right) {
        core::cmp::Ordering::Less => -1,
        core::cmp::Ordering::Equal => 0,
        core::cmp::Ordering::Greater => 1,
    };
    Ok(Value::bool(accept(ordering)))
}

/// The first offset at which `needle` occurs in `haystack`.
///
/// Written out rather than borrowed from `str`, because both sides are bytes
/// and may not be valid UTF-8 -- the point of doing this on bytes is that it
/// works whatever they hold.
fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    if needle.len() > haystack.len() {
        return None;
    }
    (0..=haystack.len() - needle.len()).find(|at| &haystack[*at..at + needle.len()] == needle)
}

/// An argument that must be a string, returned as a value rather than as text.
fn string_value_argument(vm: &Vm, at: usize, index: usize) -> Result<Value, RuntimeError> {
    let value = argument(vm, at, index);
    if !vm.is_string(value) {
        return Err(RuntimeError::new("Argument must be a string."));
    }
    Ok(value)
}

/// An argument that must be a string, with upstream's wording.
fn string_argument(
    vm: &Vm,
    at: usize,
    index: usize,
) -> Result<alloc::string::String, RuntimeError> {
    let value = argument(vm, at, index);
    if !vm.is_string(value) {
        return Err(RuntimeError::new("Argument must be a string."));
    }
    Ok(string_text(vm, value))
}

/// How many bytes the UTF-8 sequence starting with this byte occupies.
///
/// **Returns 1 for a byte that cannot start a sequence**, which is the whole
/// point: a Wren string may hold bytes that are not valid UTF-8 at all, and
/// every operation on it still has to terminate and stay inside the string.
/// Treating a stray byte as a one-byte unit is what makes that true.
fn utf8_length(lead: u8) -> usize {
    match lead {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        0xf0..=0xf7 => 4,
        _ => 1,
    }
}

/// The code point starting at `at`, or `None` if the bytes there are not a
/// valid sequence.
fn code_point_at(bytes: &[u8], at: usize) -> Option<char> {
    let length = utf8_length(*bytes.get(at)?);
    let end = at + length;
    if end > bytes.len() {
        return None;
    }
    core::str::from_utf8(&bytes[at..end]).ok()?.chars().next()
}

/// `StringByteSequence` and `StringCodePointSequence`: two views over one
/// string, differing only in what they call an element.
///
/// **Both index by byte.** That is not an oversight in the code-point view: a
/// code point *starts* at a byte, and asking for one that starts inside a
/// sequence is a real question with the answer -1. Numbering code points
/// consecutively would make `s.codePoints[i]` and `s[i]` disagree about what
/// `i` means, which is worse than an occasional -1.
fn install_string_views(vm: &mut Vm) {
    let class = vm.string_byte_sequence_class;

    define(vm, class, "count", |vm, at| {
        let string = instance_field(vm, receiver(vm, at), 0);
        Ok(Value::num(string_bytes(vm, string).len() as Num))
    });
    define(vm, class, "[_]", |vm, at| {
        let string = instance_field(vm, receiver(vm, at), 0);
        let bytes = string_bytes(vm, string);
        let index = number_argument(vm, at, 1)?;
        let index = resolve_index(index, bytes.len())?;
        Ok(Value::num(bytes[index] as Num))
    });
    define(vm, class, "iterate(_)", |vm, at| {
        let string = instance_field(vm, receiver(vm, at), 0);
        let length = string_bytes(vm, string).len();
        let current = argument(vm, at, 1);
        let next = if current.is_null() {
            0.0
        } else {
            // A negative iterator simply has no next element; it is not an
            // error, because iteration is meant to be driven blindly.
            integer_argument(vm, at, 1, "Iterator")? + 1.0
        };
        if next < 1.0 && !current.is_null() {
            return Ok(Value::FALSE);
        }
        if next < 0.0 || next >= length as Num {
            return Ok(Value::FALSE);
        }
        Ok(Value::num(next))
    });
    define(vm, class, "iteratorValue(_)", |vm, at| {
        let string = instance_field(vm, receiver(vm, at), 0);
        let bytes = string_bytes(vm, string);
        // Negative counts from the end, as every other index does.
        let index = integer_argument(vm, at, 1, "Iterator")?;
        let index = resolve_index(index, bytes.len())
            .map_err(|_| RuntimeError::new("Iterator out of bounds."))?;
        Ok(Value::num(bytes[index] as Num))
    });

    let class = vm.string_code_point_sequence_class;

    define(vm, class, "count", |vm, at| {
        let string = instance_field(vm, receiver(vm, at), 0);
        let bytes = string_bytes(vm, string);
        let count = bytes.iter().filter(|byte| !is_continuation(**byte)).count();
        Ok(Value::num(count as Num))
    });
    define(vm, class, "[_]", |vm, at| {
        let string = instance_field(vm, receiver(vm, at), 0);
        let bytes = string_bytes(vm, string);
        let index = number_argument(vm, at, 1)?;
        let index = resolve_index(index, bytes.len())?;
        Ok(Value::num(
            code_point_at(&bytes, index).map_or(-1.0, |character| character as u32 as Num),
        ))
    });
    define(vm, class, "iterate(_)", |vm, at| {
        // The same walk `String.iterate` does: one byte on, then past any
        // continuation bytes.
        let string = instance_field(vm, receiver(vm, at), 0);
        let bytes = string_bytes(vm, string);
        let current = argument(vm, at, 1);
        let mut next = if current.is_null() {
            0
        } else {
            let index = integer_argument(vm, at, 1, "Iterator")?;
            if index < 0.0 || index as usize >= bytes.len() {
                return Ok(Value::FALSE);
            }
            index as usize + 1
        };
        while next < bytes.len() && is_continuation(bytes[next]) {
            next += 1;
        }
        if next >= bytes.len() {
            return Ok(Value::FALSE);
        }
        Ok(Value::num(next as Num))
    });
    define(vm, class, "iteratorValue(_)", |vm, at| {
        let string = instance_field(vm, receiver(vm, at), 0);
        let bytes = string_bytes(vm, string);
        let index = integer_argument(vm, at, 1, "Iterator")?;
        let index = resolve_index(index, bytes.len())
            .map_err(|_| RuntimeError::new("Iterator out of bounds."))?;
        Ok(Value::num(
            code_point_at(&bytes, index).map_or(-1.0, |character| character as u32 as Num),
        ))
    });
}

/// Is this a UTF-8 continuation byte -- the second or later of a sequence?
///
/// **This, not the lead byte's implied length, is how Wren walks a string.**
/// A lead byte says how long its sequence *would* be, but the bytes after it
/// may not be continuations at all, and trusting the length would step over
/// real characters. Skipping continuations instead means a string holding
/// arbitrary bytes is walked one byte at a time exactly where it has to be.
fn is_continuation(byte: u8) -> bool {
    byte & 0xc0 == 0x80
}

/// The bytes of one character at `at`, valid sequence or not.
///
/// An invalid byte comes back as itself, so indexing a string never fails and
/// never reinterprets what it holds.
fn character_bytes(bytes: &[u8], at: usize) -> Vec<u8> {
    match code_point_at(bytes, at) {
        Some(character) => {
            let mut buffer = [0u8; 4];
            character.encode_utf8(&mut buffer).as_bytes().to_vec()
        }
        // A byte that starts no valid sequence is returned as itself, so
        // indexing a string never fails and never reinterprets what it holds.
        None => alloc::vec![bytes[at]],
    }
}

fn string_bytes(vm: &Vm, value: Value) -> Vec<u8> {
    match value.as_object().and_then(|id| vm.heap.string(id)) {
        Some(text) => text.bytes.clone(),
        _ => Vec::new(),
    }
}

fn string_text(vm: &Vm, value: Value) -> alloc::string::String {
    alloc::string::String::from_utf8_lossy(&string_bytes(vm, value)).into_owned()
}

/// `Sequence`: everything that can be iterated, and everything that follows
/// from being able to.
///
/// **The whole contract is `iterate(_)` and `iteratorValue(_)`.** A type that
/// answers those two gets `count`, `map`, `where`, `reduce`, `join` and the
/// rest for free, including a class written in Wren -- which is why these are
/// built on [`Vm::invoke_with`] rather than on any particular representation.
///
/// `List` overrides several of them with direct versions, because going
/// through the protocol to read an element it could index costs a method call
/// per element and lists are where that shows.
fn install_sequence(vm: &mut Vm) {
    let class = vm.sequence_class;

    define(vm, class, "count", |vm, at| {
        let sequence = receiver(vm, at);
        let mut count = 0.0;
        let mut iterator = Value::NULL;
        loop {
            iterator = vm.invoke_with(sequence, "iterate(_)", &[iterator])?;
            if iterator.is_falsy() {
                return Ok(Value::num(count));
            }
            count += 1.0;
        }
    });

    define(vm, class, "isEmpty", |vm, at| {
        let sequence = receiver(vm, at);
        let first = vm.invoke_with(sequence, "iterate(_)", &[Value::NULL])?;
        Ok(Value::bool(first.is_falsy()))
    });

    define(vm, class, "each(_)", |vm, at| {
        let sequence = receiver(vm, at);
        let function = argument(vm, at, 1);
        for element in collect(vm, sequence)? {
            vm.call_function(function, &[element])?;
        }
        Ok(Value::NULL)
    });

    define(vm, class, "toList", |vm, at| {
        let sequence = receiver(vm, at);
        let elements = collect(vm, sequence)?;
        Ok(new_list(vm, elements))
    });

    define(vm, class, "contains(_)", |vm, at| {
        let sequence = receiver(vm, at);
        let wanted = argument(vm, at, 1);
        for element in collect(vm, sequence)? {
            if values_equal(vm, element, wanted) {
                return Ok(Value::TRUE);
            }
        }
        Ok(Value::FALSE)
    });

    define(vm, class, "all(_)", |vm, at| {
        // **Returns the first falsy result, not `false`.** The value carries
        // more than the verdict does -- it says *which* element failed -- and
        // a program can still use it as a condition because it is falsy.
        // The last result, not `true`: `[1,2,3].all {|x| x }` is 3. Only an
        // empty sequence answers with the bare `true`, because nothing ran.
        let sequence = receiver(vm, at);
        let function = function_argument(vm, at, 1)?;
        let mut result = Value::TRUE;
        for element in collect(vm, sequence)? {
            result = vm.call_function(function, &[element])?;
            if result.is_falsy() {
                return Ok(result);
            }
        }
        Ok(result)
    });

    define(vm, class, "any(_)", |vm, at| {
        // The mirror of `all`: the first truthy result rather than `true`.
        let sequence = receiver(vm, at);
        let function = function_argument(vm, at, 1)?;
        let mut result = Value::FALSE;
        for element in collect(vm, sequence)? {
            result = vm.call_function(function, &[element])?;
            if !result.is_falsy() {
                return Ok(result);
            }
        }
        Ok(result)
    });

    define(vm, class, "reduce(_)", |vm, at| {
        let sequence = receiver(vm, at);
        let function = argument(vm, at, 1);
        expect_arity(vm, function, 2)?;
        let mut elements = collect(vm, sequence)?.into_iter();
        let Some(mut total) = elements.next() else {
            return Err(RuntimeError::new("Can't reduce an empty sequence."));
        };
        for element in elements {
            total = vm.call_function(function, &[total, element])?;
        }
        Ok(total)
    });

    define(vm, class, "reduce(_,_)", |vm, at| {
        let sequence = receiver(vm, at);
        let mut total = argument(vm, at, 1);
        let function = argument(vm, at, 2);
        expect_arity(vm, function, 2)?;
        for element in collect(vm, sequence)? {
            total = vm.call_function(function, &[total, element])?;
        }
        Ok(total)
    });

    define(vm, class, "count(_)", |vm, at| {
        let sequence = receiver(vm, at);
        let function = function_argument(vm, at, 1)?;
        let mut count = 0.0;
        for element in collect(vm, sequence)? {
            if !vm.call_function(function, &[element])?.is_falsy() {
                count += 1.0;
            }
        }
        Ok(Value::num(count))
    });

    define(vm, class, "join()", |vm, at| {
        let sequence = receiver(vm, at);
        let joined = join_sequence(vm, sequence, "")?;
        Ok(vm.new_string(&joined))
    });
    define(vm, class, "join(_)", |vm, at| {
        let sequence = receiver(vm, at);
        let separator = argument(vm, at, 1);
        if !vm.is_string(separator) {
            return Err(RuntimeError::new("Right operand must be a string."));
        }
        let separator = vm.to_string(separator);
        let joined = join_sequence(vm, sequence, &separator)?;
        Ok(vm.new_string(&joined))
    });

    // **The lazy four.** `map` on an infinite sequence has to stay infinite,
    // so these build a view rather than a list. Upstream's own test drives
    // `map` with a Fibonacci iterator that never ends.
    define(vm, class, "map(_)", |vm, at| {
        let function = function_argument(vm, at, 1)?;
        let class = vm.map_sequence_class;
        Ok(new_view(vm, class, &[receiver(vm, at), function]))
    });
    define(vm, class, "where(_)", |vm, at| {
        let function = function_argument(vm, at, 1)?;
        let class = vm.where_sequence_class;
        Ok(new_view(vm, class, &[receiver(vm, at), function]))
    });
    define(vm, class, "take(_)", |vm, at| {
        let count = counting_argument(vm, at, 1)?;
        let class = vm.take_sequence_class;
        Ok(new_view(
            vm,
            class,
            &[receiver(vm, at), Value::num(count), Value::num(0.0)],
        ))
    });
    define(vm, class, "skip(_)", |vm, at| {
        let count = counting_argument(vm, at, 1)?;
        let class = vm.skip_sequence_class;
        Ok(new_view(vm, class, &[receiver(vm, at), Value::num(count)]))
    });

    install_lazy_sequences(vm);
}

/// The lazy views. Each holds its source and delegates the protocol to it.
fn install_lazy_sequences(vm: &mut Vm) {
    // `map`: same iteration, transformed values.
    let class = vm.map_sequence_class;
    define(vm, class, "iterate(_)", |vm, at| {
        let source = instance_field(vm, receiver(vm, at), 0);
        let iterator = argument(vm, at, 1);
        vm.invoke_with(source, "iterate(_)", &[iterator])
    });
    define(vm, class, "iteratorValue(_)", |vm, at| {
        let source = instance_field(vm, receiver(vm, at), 0);
        let function = instance_field(vm, receiver(vm, at), 1);
        let iterator = argument(vm, at, 1);
        let value = vm.invoke_with(source, "iteratorValue(_)", &[iterator])?;
        vm.call_function(function, &[value])
    });

    // `where`: same values, advancing past the ones that do not match.
    let class = vm.where_sequence_class;
    define(vm, class, "iterate(_)", |vm, at| {
        let source = instance_field(vm, receiver(vm, at), 0);
        let function = instance_field(vm, receiver(vm, at), 1);
        let mut iterator = argument(vm, at, 1);
        loop {
            iterator = vm.invoke_with(source, "iterate(_)", &[iterator])?;
            if iterator.is_falsy() {
                return Ok(Value::FALSE);
            }
            let value = vm.invoke_with(source, "iteratorValue(_)", &[iterator])?;
            if !vm.call_function(function, &[value])?.is_falsy() {
                return Ok(iterator);
            }
        }
    });
    define(vm, class, "iteratorValue(_)", |vm, at| {
        let source = instance_field(vm, receiver(vm, at), 0);
        let iterator = argument(vm, at, 1);
        vm.invoke_with(source, "iteratorValue(_)", &[iterator])
    });

    // `take`: stops after a count, which it has to remember between calls --
    // hence the third field rather than deriving it from the iterator, which
    // for an arbitrary sequence is not a number this can count with.
    let class = vm.take_sequence_class;
    define(vm, class, "iterate(_)", |vm, at| {
        let this = receiver(vm, at);
        let source = instance_field(vm, this, 0);
        let limit = instance_field(vm, this, 1).as_num().unwrap_or(0.0);
        let iterator = argument(vm, at, 1);

        let taken = if iterator.is_null() {
            1.0
        } else {
            instance_field(vm, this, 2).as_num().unwrap_or(0.0) + 1.0
        };
        set_instance_field(vm, this, 2, Value::num(taken));
        if taken > limit {
            return Ok(Value::FALSE);
        }
        vm.invoke_with(source, "iterate(_)", &[iterator])
    });
    define(vm, class, "iteratorValue(_)", |vm, at| {
        let source = instance_field(vm, receiver(vm, at), 0);
        let iterator = argument(vm, at, 1);
        vm.invoke_with(source, "iteratorValue(_)", &[iterator])
    });

    // `skip`: burns through the first n on the first call only.
    let class = vm.skip_sequence_class;
    define(vm, class, "iterate(_)", |vm, at| {
        let this = receiver(vm, at);
        let source = instance_field(vm, this, 0);
        let mut iterator = argument(vm, at, 1);

        if !iterator.is_null() {
            return vm.invoke_with(source, "iterate(_)", &[iterator]);
        }
        let mut remaining = instance_field(vm, this, 1).as_num().unwrap_or(0.0);
        iterator = vm.invoke_with(source, "iterate(_)", &[iterator])?;
        while remaining > 0.0 && !iterator.is_falsy() {
            iterator = vm.invoke_with(source, "iterate(_)", &[iterator])?;
            remaining -= 1.0;
        }
        Ok(iterator)
    });
    define(vm, class, "iteratorValue(_)", |vm, at| {
        let source = instance_field(vm, receiver(vm, at), 0);
        let iterator = argument(vm, at, 1);
        vm.invoke_with(source, "iteratorValue(_)", &[iterator])
    });
}

/// Walk a sequence into a list, through the protocol.
///
/// **This is what makes an infinite sequence hang**, which is correct: asking
/// an endless sequence for its elements is a question with no answer, and the
/// lazy views exist precisely so the question is not asked.
fn collect(vm: &mut Vm, sequence: Value) -> Result<Vec<Value>, RuntimeError> {
    let mut elements = Vec::new();
    let mut iterator = Value::NULL;
    loop {
        iterator = vm.invoke_with(sequence, "iterate(_)", &[iterator])?;
        if iterator.is_falsy() {
            return Ok(elements);
        }
        elements.push(vm.invoke_with(sequence, "iteratorValue(_)", &[iterator])?);
    }
}

fn join_sequence(
    vm: &mut Vm,
    sequence: Value,
    separator: &str,
) -> Result<alloc::string::String, RuntimeError> {
    let mut out = alloc::string::String::new();
    for (index, element) in collect(vm, sequence)?.into_iter().enumerate() {
        if index > 0 {
            out.push_str(separator);
        }
        out.push_str(&vm.stringify(element)?);
    }
    Ok(out)
}

fn new_view(vm: &mut Vm, class: crate::handle::ObjectId, fields: &[Value]) -> Value {
    let id = vm.heap.allocate(Object::Instance(ObjInstance {
        class,
        fields: fields.to_vec(),
    }));
    Value::object(id)
}

fn set_instance_field(vm: &mut Vm, value: Value, index: usize, to: Value) {
    if let Some(instance) = value.as_object().and_then(|id| vm.heap.instance_mut(id)) {
        if instance.fields.len() <= index {
            instance.fields.resize(index + 1, Value::NULL);
        }
        instance.fields[index] = to;
    }
}

fn function_argument(vm: &Vm, at: usize, index: usize) -> Result<Value, RuntimeError> {
    let value = argument(vm, at, index);
    match value.as_object().and_then(|id| vm.heap.closure(id)) {
        Some(_) => Ok(value),
        None => Err(RuntimeError::new("Argument must be a function.")),
    }
}

/// A count for `take` or `skip`: a non-negative whole number.
fn counting_argument(vm: &Vm, at: usize, index: usize) -> Result<Num, RuntimeError> {
    let count = integer_argument(vm, at, index, "Count")?;
    if count < 0.0 {
        return Err(RuntimeError::new("Count must be a non-negative integer."));
    }
    Ok(count)
}

fn install_list(vm: &mut Vm) {
    let class = vm.list_class;

    // `List` has to be reachable by name, because a list literal compiles to
    // `List.new` followed by an `addCore(_)` per element rather than to an
    // opcode of its own.
    let metaclass_name = vm
        .heap
        .allocate(Object::String(ObjString::from_text("List metaclass")));
    let metaclass = vm.heap.allocate(Object::Class(Box::new(ObjClass::new(
        metaclass_name,
        Some(vm.class_class),
    ))));
    if let Some(list) = vm.heap.class_mut(class) {
        list.metaclass = Some(metaclass);
    }
    define(vm, metaclass, "new()", |vm, _| Ok(new_list(vm, Vec::new())));
    vm.modules[0].define("List", Value::object(class));

    // Like `add(_)`, but returns the *list* rather than the element, so a list
    // literal can add each element without reloading the list between them.
    define(vm, class, "addCore(_)", |vm, at| {
        let list = receiver(vm, at);
        let element = argument(vm, at, 1);
        match vm.heap.list_mut(list.as_object().unwrap()) {
            Some(list) => list.elements.push(element),
            _ => return Err(RuntimeError::new("Receiver must be a list.")),
        }
        Ok(list)
    });

    define(vm, class, "add(_)", |vm, at| {
        let list = receiver(vm, at);
        let element = argument(vm, at, 1);
        match vm.heap.list_mut(list.as_object().unwrap()) {
            Some(list) => list.elements.push(element),
            _ => return Err(RuntimeError::new("Receiver must be a list.")),
        }
        // `add` returns the element, which is what makes `list.add(x)` usable
        // as an expression.
        Ok(element)
    });

    define(vm, class, "count", |vm, at| {
        Ok(Value::num(list_length(vm, receiver(vm, at)) as Num))
    });

    define(vm, class, "[_]", |vm, at| {
        let list = receiver(vm, at);
        let length = list_length(vm, list);

        // **A range subscript is a slice**, which is why this cannot simply
        // demand a number: `list[1..2]` is a list, `list[1]` is an element.
        if let Some(range) = range_of(vm, argument(vm, at, 1)) {
            let taken = slice_indices(&range, length)?;
            let elements = list_elements(vm, list);
            let sliced = taken.into_iter().map(|index| elements[index]).collect();
            return Ok(new_list(vm, sliced));
        }

        let index = number_argument(vm, at, 1)?;
        let index = resolve_index(index, length)?;
        match vm.heap.list(list.as_object().unwrap()) {
            Some(list) => Ok(list.elements[index]),
            _ => Err(RuntimeError::new("Receiver must be a list.")),
        }
    });

    define(vm, class, "[_]=(_)", |vm, at| {
        let list = receiver(vm, at);
        let index = number_argument(vm, at, 1)?;
        let value = argument(vm, at, 2);
        let length = list_length(vm, list);
        let index = resolve_index(index, length)?;
        match vm.heap.list_mut(list.as_object().unwrap()) {
            Some(list) => {
                list.elements[index] = value;
                Ok(value)
            }
            _ => Err(RuntimeError::new("Receiver must be a list.")),
        }
    });

    // The iteration protocol. `for (x in list)` compiles to calls to these two,
    // so a type is iterable exactly when it answers them — there is no separate
    // iterator object and nothing to allocate per loop.
    define(vm, class, "iterate(_)", |vm, at| {
        let length = list_length(vm, receiver(vm, at));
        if length == 0 {
            return Ok(Value::FALSE);
        }
        let current = argument(vm, at, 1);
        if current.is_null() {
            return Ok(Value::num(0.0));
        }
        let index = integer_argument(vm, at, 1, "Iterator")?;
        if index < 0.0 || index >= (length - 1) as Num {
            return Ok(Value::FALSE);
        }
        Ok(Value::num(index + 1.0))
    });

    define(vm, class, "iteratorValue(_)", |vm, at| {
        let list = receiver(vm, at);
        let index = number_argument(vm, at, 1)?;
        let length = list_length(vm, list);
        let index = resolve_index(index, length)?;
        match vm.heap.list(list.as_object().unwrap()) {
            Some(list) => Ok(list.elements[index]),
            _ => Err(RuntimeError::new("Receiver must be a list.")),
        }
    });

    define(vm, class, "toString", |vm, at| {
        // **Each element through its own `toString`**, not through the VM's
        // formatter: a class defining `toString` should have it used inside a
        // list too, which is what upstream's core/list/to_string checks.
        let mut out = alloc::string::String::from("[");
        for (index, element) in list_elements(vm, receiver(vm, at)).into_iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            out.push_str(&vm.stringify(element)?);
        }
        out.push(']');
        Ok(vm.new_string(&out))
    });
}

/// The rest of `List`, and the `Sequence` methods it inherits upstream.
///
/// **Upstream writes these in Wren**, in `wren_core.wren`, as methods on a
/// `Sequence` class that `List`, `Map`, `Range` and `String` all inherit. That
/// costs a compile of several hundred lines at every start-up — 33 KB of
/// compiler stack on the C6 — so here they are primitives on the types that
/// need them.
fn install_list_extras(vm: &mut Vm) {
    let class = vm.list_class;

    define(vm, class, "toList", |vm, at| Ok(receiver(vm, at)));

    define(vm, class, "insert(_,_)", |vm, at| {
        let list = receiver(vm, at);
        let index = integer_argument(vm, at, 1, "Index")?;
        let value = argument(vm, at, 2);
        let length = list_length(vm, list);
        // `insert` accepts one past the end, where the other index-taking
        // methods do not: appending is a legitimate insertion point.
        let at_index = if index < 0.0 {
            index + length as Num + 1.0
        } else {
            index
        };
        if at_index < 0.0 || at_index > length as Num {
            return Err(RuntimeError::new("Index out of bounds."));
        }
        match vm.heap.list_mut(list.as_object().unwrap()) {
            Some(list) => {
                list.elements.insert(at_index as usize, value);
                Ok(value)
            }
            _ => Err(RuntimeError::new("Receiver must be a list.")),
        }
    });

    define(vm, class, "+(_)", |vm, at| {
        let mut joined = list_elements(vm, receiver(vm, at));
        let other = argument(vm, at, 1);
        // **Any sequence on the right**, not just a list: `[1, 2] + "abc"`
        // appends the characters and `[1] + (2..3)` the numbers. Requiring a
        // list would be a narrower language than Wren.
        joined.extend(collect(vm, other)?);
        Ok(new_list(vm, joined))
    });

    define(vm, class, "addAll(_)", |vm, at| {
        let list = receiver(vm, at);
        let other = argument(vm, at, 1);
        // Any sequence, not just a list: `list.addAll(1..3)` is ordinary Wren.
        let added = collect(vm, other)?;
        match vm.heap.list_mut(list.as_object().unwrap()) {
            Some(list) => list.elements.extend(added),
            _ => return Err(RuntimeError::new("Receiver must be a list.")),
        }
        Ok(other)
    });

    define(vm, class, "remove(_)", |vm, at| {
        let list = receiver(vm, at);
        let wanted = argument(vm, at, 1);
        let found = list_elements(vm, list)
            .iter()
            .position(|element| values_equal(vm, *element, wanted));
        match (found, vm.heap.list_mut(list.as_object().unwrap())) {
            (Some(index), Some(list)) => Ok(list.elements.remove(index)),
            // Removing something that is not there answers null rather than
            // failing, which is what makes `remove` usable without a
            // `contains` in front of it.
            _ => Ok(Value::NULL),
        }
    });

    define(vm, class, "*(_)", |vm, at| {
        let count = integer_argument(vm, at, 1, "Count")?;
        if count < 0.0 {
            return Err(RuntimeError::new("Count must be a non-negative integer."));
        }
        let elements = list_elements(vm, receiver(vm, at));
        let mut repeated = Vec::with_capacity(elements.len() * count as usize);
        for _ in 0..count as usize {
            repeated.extend(elements.iter().copied());
        }
        Ok(new_list(vm, repeated))
    });

    define(vm, class, "removeAt(_)", |vm, at| {
        let list = receiver(vm, at);
        let index = number_argument(vm, at, 1)?;
        let length = list_length(vm, list);
        let index = resolve_index(index, length)?;
        match vm.heap.list_mut(list.as_object().unwrap()) {
            Some(list) => Ok(list.elements.remove(index)),
            _ => Err(RuntimeError::new("Receiver must be a list.")),
        }
    });

    define(vm, class, "clear()", |vm, at| {
        if let Some(list) = receiver(vm, at)
            .as_object()
            .and_then(|id| vm.heap.list_mut(id))
        {
            list.elements.clear();
        }
        Ok(Value::NULL)
    });

    define(vm, class, "contains(_)", |vm, at| {
        let wanted = argument(vm, at, 1);
        let found = list_elements(vm, receiver(vm, at))
            .iter()
            .any(|element| values_equal(vm, *element, wanted));
        Ok(Value::bool(found))
    });

    define(vm, class, "indexOf(_)", |vm, at| {
        let wanted = argument(vm, at, 1);
        let found = list_elements(vm, receiver(vm, at))
            .iter()
            .position(|element| values_equal(vm, *element, wanted));
        Ok(Value::num(found.map_or(-1.0, |index| index as Num)))
    });

    define(vm, class, "isEmpty", |vm, at| {
        Ok(Value::bool(list_length(vm, receiver(vm, at)) == 0))
    });

    define(vm, class, "each(_)", |vm, at| {
        let function = argument(vm, at, 1);
        for element in list_elements(vm, receiver(vm, at)) {
            vm.call_function(function, &[element])?;
        }
        Ok(Value::NULL)
    });

    define(vm, class, "map(_)", |vm, at| {
        let function = argument(vm, at, 1);
        let mut mapped = Vec::new();
        for element in list_elements(vm, receiver(vm, at)) {
            mapped.push(vm.call_function(function, &[element])?);
        }
        Ok(new_list(vm, mapped))
    });

    define(vm, class, "where(_)", |vm, at| {
        let function = argument(vm, at, 1);
        let mut kept = Vec::new();
        for element in list_elements(vm, receiver(vm, at)) {
            if !vm.call_function(function, &[element])?.is_falsy() {
                kept.push(element);
            }
        }
        Ok(new_list(vm, kept))
    });

    // `all` and `any` are not defined here: `Sequence` has them, and its
    // versions return the deciding element rather than a boolean. A direct
    // copy on `List` would have been faster and wrong.

    define(vm, class, "reduce(_)", |vm, at| {
        let function = argument(vm, at, 1);
        expect_arity(vm, function, 2)?;
        let elements = list_elements(vm, receiver(vm, at));
        let mut iterator = elements.into_iter();
        let Some(mut total) = iterator.next() else {
            return Err(RuntimeError::new("Can't reduce an empty sequence."));
        };
        for element in iterator {
            total = vm.call_function(function, &[total, element])?;
        }
        Ok(total)
    });

    define(vm, class, "reduce(_,_)", |vm, at| {
        let mut total = argument(vm, at, 1);
        let function = argument(vm, at, 2);
        expect_arity(vm, function, 2)?;
        for element in list_elements(vm, receiver(vm, at)) {
            total = vm.call_function(function, &[total, element])?;
        }
        Ok(total)
    });

    define(vm, class, "join()", |vm, at| {
        let joined = join_elements(vm, receiver(vm, at), "")?;
        Ok(vm.new_string(&joined))
    });

    define(vm, class, "join(_)", |vm, at| {
        let separator = argument(vm, at, 1);
        if !vm.is_string(separator) {
            return Err(RuntimeError::new("Right operand must be a string."));
        }
        let separator = vm.to_string(separator);
        let joined = join_elements(vm, receiver(vm, at), &separator)?;
        Ok(vm.new_string(&joined))
    });

    // `List.new()` and `List.filled(n, value)`.
    let metaclass = match vm.heap.class(class) {
        Some(list) => list.metaclass,
        _ => None,
    };
    if let Some(metaclass) = metaclass {
        define(vm, metaclass, "filled(_,_)", |vm, at| {
            let count = number_argument(vm, at, 1)?;
            if count < 0.0 || count != math::trunc(count) {
                return Err(RuntimeError::new("Size must be a non-negative integer."));
            }
            let value = argument(vm, at, 2);
            Ok(new_list(vm, alloc::vec![value; count as usize]))
        });
    }
}

fn list_elements(vm: &Vm, list: Value) -> Vec<Value> {
    match list.as_object().and_then(|id| vm.heap.list(id)) {
        Some(list) => list.elements.clone(),
        _ => Vec::new(),
    }
}

fn join_elements(
    vm: &mut Vm,
    list: Value,
    separator: &str,
) -> Result<alloc::string::String, RuntimeError> {
    let mut out = alloc::string::String::new();
    for (index, element) in list_elements(vm, list).into_iter().enumerate() {
        if index > 0 {
            out.push_str(separator);
        }
        out.push_str(&vm.stringify(element)?);
    }
    Ok(out)
}

fn list_length(vm: &Vm, list: Value) -> usize {
    match list.as_object().and_then(|id| vm.heap.list(id)) {
        Some(list) => list.elements.len(),
        _ => 0,
    }
}

/// Turn a possibly-negative index into a real one, as Wren does.
///
/// `list[-1]` is the last element. Upstream's message for an out-of-range index
/// is exactly this, and the suite checks it.
fn resolve_index(index: Num, length: usize) -> Result<usize, RuntimeError> {
    if index != math::trunc(index) {
        return Err(RuntimeError::new("Index must be an integer."));
    }
    let resolved = if index < 0.0 {
        index + length as Num
    } else {
        index
    };
    if resolved < 0.0 || resolved >= length as Num {
        return Err(RuntimeError::new("Index out of bounds."));
    }
    Ok(resolved as usize)
}

/// `Map`, and the `MapEntry` its iteration yields.
fn install_map(vm: &mut Vm) {
    let class = vm.map_class;

    let metaclass_name = vm
        .heap
        .allocate(Object::String(ObjString::from_text("Map metaclass")));
    let metaclass = vm.heap.allocate(Object::Class(Box::new(ObjClass::new(
        metaclass_name,
        Some(vm.class_class),
    ))));
    if let Some(map) = vm.heap.class_mut(class) {
        map.metaclass = Some(metaclass);
    }
    define(vm, metaclass, "new()", |vm, _| {
        let id = vm.heap.allocate(Object::Map(ObjMap::new()));
        Ok(Value::object(id))
    });

    // What a map literal emits per entry; returns the map so the next entry
    // does not have to reload it.
    define(vm, class, "addCore(_,_)", |vm, at| {
        let map = receiver(vm, at);
        let key = argument(vm, at, 1);
        let value = argument(vm, at, 2);
        map_set(vm, map, key, value)?;
        Ok(map)
    });

    define(vm, class, "[_]", |vm, at| {
        let map = receiver(vm, at);
        let key = value_type_argument(vm, at, 1)?;
        // **A missing key is null, not an error.** That is Wren's rule and it
        // is what makes `map[k] ?: default` idiomatic.
        Ok(map_get(vm, map, key).unwrap_or(Value::NULL))
    });

    define(vm, class, "[_]=(_)", |vm, at| {
        let map = receiver(vm, at);
        let key = value_type_argument(vm, at, 1)?;
        let value = argument(vm, at, 2);
        map_set(vm, map, key, value)?;
        Ok(value)
    });

    define(vm, class, "count", |vm, at| {
        Ok(Value::num(map_count(vm, receiver(vm, at)) as Num))
    });

    define(vm, class, "containsKey(_)", |vm, at| {
        let map = receiver(vm, at);
        let key = value_type_argument(vm, at, 1)?;
        Ok(Value::bool(map_get(vm, map, key).is_some()))
    });

    define(vm, class, "remove(_)", |vm, at| {
        let map = receiver(vm, at);
        let key = value_type_argument(vm, at, 1)?;
        let Some(id) = map.as_object() else {
            return Err(RuntimeError::new("Receiver must be a map."));
        };
        let found = map_index(vm, map, key);
        match (found, vm.heap.map_mut(id)) {
            (Some(slot), Some(map)) => {
                let removed = map.entries[slot].value;
                // **A tombstone, not an empty slot.** A key that collided with
                // this one probed past it on the way in; blanking the slot
                // would end that probe early and lose the key entirely.
                map.entries[slot] = MapEntry {
                    key: Value::UNDEFINED,
                    value: Value::TRUE,
                };
                map.count -= 1;
                Ok(removed)
            }
            _ => Ok(Value::NULL),
        }
    });

    define(vm, class, "clear()", |vm, at| {
        if let Some(map) = receiver(vm, at)
            .as_object()
            .and_then(|id| vm.heap.map_mut(id))
        {
            map.entries.clear();
            map.count = 0;
        }
        Ok(Value::NULL)
    });

    define(vm, class, "toString", |vm, at| {
        // `{}` when empty, `{key: value, ...}` otherwise -- the literal syntax,
        // so what a map prints can be pasted back into a program.
        let entries = map_entries(vm, receiver(vm, at));
        let mut out = alloc::string::String::from("{");
        for (index, entry) in entries.into_iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            out.push_str(&vm.stringify(entry.key)?);
            out.push_str(": ");
            out.push_str(&vm.stringify(entry.value)?);
        }
        out.push('}');
        Ok(vm.new_string(&out))
    });

    // **Views over the same table, not copies.** `map.keys` yields the keys as
    // the map is iterated, so the slot indices a program sees match the map's
    // own -- and building a list would snapshot a map that may still change.
    define(vm, class, "keys", |vm, at| {
        let class = vm.map_key_sequence_class;
        Ok(new_view(vm, class, &[receiver(vm, at)]))
    });
    define(vm, class, "values", |vm, at| {
        let class = vm.map_value_sequence_class;
        Ok(new_view(vm, class, &[receiver(vm, at)]))
    });

    // Iterating a map yields `MapEntry` objects, so `for (e in map)` can reach
    // both halves through `e.key` and `e.value`.
    define(vm, class, "iterate(_)", |vm, at| {
        // **Slot indices, not entry numbers.** The table is sparse, so
        // iterating means finding the next occupied slot -- which is exactly
        // what a program sees, and what upstream's tests check.
        let map = receiver(vm, at);
        let current = argument(vm, at, 1);
        let from = if current.is_null() {
            0
        } else {
            let index = integer_argument(vm, at, 1, "Iterator")?;
            if index < 0.0 {
                return Ok(Value::FALSE);
            }
            index as usize + 1
        };
        let next = match map.as_object().and_then(|id| vm.heap.map(id)) {
            Some(map) => map.next_live(from),
            _ => return Err(RuntimeError::new("Receiver must be a map.")),
        };
        Ok(next.map_or(Value::FALSE, |slot| Value::num(slot as Num)))
    });

    define(vm, class, "iteratorValue(_)", |vm, at| {
        let map = receiver(vm, at);
        let slot = integer_argument(vm, at, 1, "Iterator")?;
        let capacity = match map.as_object().and_then(|id| vm.heap.map(id)) {
            Some(map) => map.entries.len(),
            _ => return Err(RuntimeError::new("Receiver must be a map.")),
        };
        // **Out of the table's range and pointing at an empty slot are
        // different faults.** The first is a bad index, the second an iterator
        // that has gone stale, and upstream reports them differently.
        if slot < 0.0 || slot as usize >= capacity {
            return Err(RuntimeError::new("Iterator out of bounds."));
        }
        let slot = slot as usize;
        let entry = match map.as_object().and_then(|id| vm.heap.map(id)) {
            Some(map) if map.is_live(slot) => map.entries[slot],
            _ => return Err(RuntimeError::new("Invalid map iterator.")),
        };
        let class = vm.map_entry_class;
        let id = vm.heap.allocate(Object::Instance(ObjInstance {
            class,
            fields: alloc::vec![entry.key, entry.value],
        }));
        Ok(Value::object(id))
    });

    // The two views delegate iteration to the map and take one half of each
    // entry. `field 0` is the map they are over.
    // Both advance the underlying map identically; only the half they read
    // out of each entry differs, which is the `iteratorValue` below.
    for class in [vm.map_key_sequence_class, vm.map_value_sequence_class] {
        define(vm, class, "iterate(_)", |vm, at| {
            let map = instance_field(vm, receiver(vm, at), 0);
            let iterator = argument(vm, at, 1);
            vm.invoke_with(map, "iterate(_)", &[iterator])
        });
    }
    define(
        vm,
        vm.map_key_sequence_class,
        "iteratorValue(_)",
        |vm, at| {
            let map = instance_field(vm, receiver(vm, at), 0);
            let iterator = argument(vm, at, 1);
            let entry = vm.invoke_with(map, "iteratorValue(_)", &[iterator])?;
            Ok(instance_field(vm, entry, 0))
        },
    );
    define(
        vm,
        vm.map_value_sequence_class,
        "iteratorValue(_)",
        |vm, at| {
            let map = instance_field(vm, receiver(vm, at), 0);
            let iterator = argument(vm, at, 1);
            let entry = vm.invoke_with(map, "iteratorValue(_)", &[iterator])?;
            Ok(instance_field(vm, entry, 1))
        },
    );

    // `MapEntry` itself: two fields, reachable by name.
    let entry_class = vm.map_entry_class;
    let entry_metaclass_name = vm
        .heap
        .allocate(Object::String(ObjString::from_text("MapEntry metaclass")));
    let entry_metaclass = vm.heap.allocate(Object::Class(Box::new(ObjClass::new(
        entry_metaclass_name,
        Some(vm.class_class),
    ))));
    if let Some(entry) = vm.heap.class_mut(entry_class) {
        entry.metaclass = Some(entry_metaclass);
    }
    define(vm, entry_metaclass, "new(_,_)", |vm, at| {
        let key = argument(vm, at, 1);
        let value = argument(vm, at, 2);
        let class = vm.map_entry_class;
        let id = vm.heap.allocate(Object::Instance(ObjInstance {
            class,
            fields: alloc::vec![key, value],
        }));
        Ok(Value::object(id))
    });

    define(vm, entry_class, "key", |vm, at| {
        Ok(instance_field(vm, receiver(vm, at), 0))
    });
    define(vm, entry_class, "value", |vm, at| {
        Ok(instance_field(vm, receiver(vm, at), 1))
    });
    define(vm, entry_class, "toString", |vm, at| {
        let key = instance_field(vm, receiver(vm, at), 0);
        let value = instance_field(vm, receiver(vm, at), 1);
        let text = alloc::format!("{}:{}", vm.to_string(key), vm.to_string(value));
        Ok(vm.new_string(&text))
    });
}

fn instance_field(vm: &Vm, value: Value, index: usize) -> Value {
    match value.as_object().and_then(|id| vm.heap.instance(id)) {
        Some(instance) => instance.fields.get(index).copied().unwrap_or(Value::NULL),
        _ => Value::NULL,
    }
}

/// The live entries of a map, in slot order.
fn map_entries(vm: &Vm, map: Value) -> Vec<MapEntry> {
    match map.as_object().and_then(|id| vm.heap.map(id)) {
        Some(map) => map.live().copied().collect(),
        _ => Vec::new(),
    }
}

/// How many live entries a map has.
fn map_count(vm: &Vm, map: Value) -> usize {
    match map.as_object().and_then(|id| vm.heap.map(id)) {
        Some(map) => map.count,
        _ => 0,
    }
}

/// The hash of a value that may be used as a map key.
///
/// **Only the value types reach here**, which is what makes hashing by
/// contents safe: a mutable object could change after insertion and be lost in
/// its own table.
fn hash_value(vm: &Vm, value: Value) -> u32 {
    if let Some(number) = value.as_num() {
        // Hash the bits, folded, so that nearby numbers do not all land in
        // nearby slots. `0.0` and `-0.0` are equal under `==` and must hash
        // alike, so the sign of zero is normalised away first.
        // Widened to 64 bits before folding: the shift below is an overflow
        // when a `Num` is 32 bits wide, and the fold is then a no-op rather
        // than a panic.
        let normalised = if number == 0.0 { 0.0 } else { number };
        // Redundant in a 64-bit build and required in a 32-bit one, which is
        // why the lint is silenced rather than the cast removed.
        #[allow(clippy::unnecessary_cast)]
        let bits = normalised.to_bits() as u64;
        return (bits as u32) ^ ((bits >> 32) as u32);
    }
    if value.is_null() {
        return 1;
    }
    if value.is_true() {
        return 2;
    }
    if value.is_false() {
        return 3;
    }
    match value.as_object().and_then(|id| vm.heap.get(id)) {
        // The cached hash, which is the reason it is cached.
        Some(Object::String(text)) => text.hash(),
        Some(Object::Range(range)) => {
            // Widened before folding: the shift below is an overflow when a
            // `Num` is 32 bits, where the fold should simply do nothing.
            #[allow(clippy::unnecessary_cast)]
            let from = range.from.to_bits() as u64;
            #[allow(clippy::unnecessary_cast)]
            let to = range.to.to_bits() as u64;
            (from as u32)
                ^ ((from >> 32) as u32)
                ^ (to as u32).rotate_left(7)
                ^ u32::from(range.is_inclusive)
        }
        // A class is identified by which object it is, so its handle is its
        // identity and hashing it is enough.
        _ => value
            .as_object()
            .map_or(0, |id| id.raw())
            .wrapping_mul(2654435761),
    }
}

/// Where `key` belongs in a table of this capacity.
///
/// Returns the slot holding the key if it is present, otherwise the first slot
/// it could be inserted into. **The first tombstone seen is remembered and
/// preferred**, so that repeated insert-and-remove cycles reuse slots instead
/// of lengthening every probe behind them.
fn probe(vm: &Vm, entries: &[MapEntry], key: Value) -> (usize, bool) {
    let capacity = entries.len();
    let mask = capacity - 1;
    let mut slot = (hash_value(vm, key) as usize) & mask;
    let mut tombstone: Option<usize> = None;

    loop {
        let entry = entries[slot];
        if entry.key.is_undefined() {
            // A false value marks a slot never used, so the probe ends: no key
            // was ever placed beyond it by a collision.
            if entry.value.is_falsy() {
                return (tombstone.unwrap_or(slot), false);
            }
            if tombstone.is_none() {
                tombstone = Some(slot);
            }
        } else if values_equal(vm, entry.key, key) {
            return (slot, true);
        }
        slot = (slot + 1) & mask;
    }
}

/// Grow the table, rehashing every live entry into the new one.
fn grow_map(vm: &mut Vm, id: crate::handle::ObjectId, wanted: usize) {
    let mut capacity = 8;
    while capacity < wanted {
        capacity *= 2;
    }

    let old = match vm.heap.map(id) {
        Some(map) => map.entries.clone(),
        _ => return,
    };
    let empty = MapEntry {
        key: Value::UNDEFINED,
        value: Value::FALSE,
    };
    let mut entries = alloc::vec![empty; capacity];

    let mut count = 0;
    for entry in old.iter().filter(|entry| !entry.key.is_undefined()) {
        let (slot, _) = probe(vm, &entries, entry.key);
        entries[slot] = *entry;
        count += 1;
    }

    if let Some(map) = vm.heap.map_mut(id) {
        map.entries = entries;
        map.count = count;
    }
}

/// The slot holding `key`, if the map has it.
fn map_index(vm: &Vm, map: Value, key: Value) -> Option<usize> {
    let entries = match map.as_object().and_then(|id| vm.heap.map(id)) {
        Some(map) if !map.entries.is_empty() => &map.entries,
        _ => return None,
    };
    match probe(vm, entries, key) {
        (slot, true) => Some(slot),
        (_, false) => None,
    }
}

fn map_get(vm: &Vm, map: Value, key: Value) -> Option<Value> {
    let slot = map_index(vm, map, key)?;
    match map.as_object().and_then(|id| vm.heap.map(id)) {
        Some(map) => map.entries.get(slot).map(|entry| entry.value),
        _ => None,
    }
}

fn map_set(vm: &mut Vm, map: Value, key: Value, value: Value) -> Result<(), RuntimeError> {
    let Some(id) = map.as_object() else {
        return Err(RuntimeError::new("Receiver must be a map."));
    };

    // **Grown at three quarters full.** A linear-probing table degrades sharply
    // as it fills: the probe length climbs with the square of the load, so the
    // last few insertions into a full table cost more than all the rest.
    let (count, capacity) = match vm.heap.map(id) {
        Some(map) => (map.count, map.entries.len()),
        _ => return Err(RuntimeError::new("Receiver must be a map.")),
    };
    if capacity == 0 || (count + 1) * 4 > capacity * 3 {
        grow_map(vm, id, (count + 1) * 2);
    }

    let entries = match vm.heap.map(id) {
        Some(map) => map.entries.clone(),
        _ => return Err(RuntimeError::new("Receiver must be a map.")),
    };
    let (slot, existing) = probe(vm, &entries, key);

    if let Some(map) = vm.heap.map_mut(id) {
        map.entries[slot] = MapEntry { key, value };
        if !existing {
            map.count += 1;
        }
    }
    Ok(())
}

/// Wren's `==` for the types that can be map keys.
///
/// **Strings compare by contents**, which is the whole reason this is not just
/// a bit comparison: two separately allocated `"a"` are the same key.
pub fn values_equal(vm: &Vm, left: Value, right: Value) -> bool {
    if left.is_same(right) {
        return true;
    }
    if let (Some(a), Some(b)) = (left.as_num(), right.as_num()) {
        return a == b;
    }
    // **Ranges compare by value.** Two separately built `1..3` are the same
    // range, and since a range is a legal map key, comparing them by identity
    // meant `map[1..3]` could never find what `map[1..3] = x` had put there.
    if let (Some(a), Some(b)) = (range_of(vm, left), range_of(vm, right)) {
        return a.from == b.from && a.to == b.to && a.is_inclusive == b.is_inclusive;
    }
    strings_equal(vm, left, right)
}

fn install_range(vm: &mut Vm) {
    let class = vm.range_class;

    define(vm, class, "from", |vm, at| {
        Ok(Value::num(
            range_of(vm, receiver(vm, at)).map_or(Num::NAN, |r| r.from),
        ))
    });
    define(vm, class, "to", |vm, at| {
        Ok(Value::num(
            range_of(vm, receiver(vm, at)).map_or(Num::NAN, |r| r.to),
        ))
    });
    define(vm, class, "min", |vm, at| {
        let range = range_of(vm, receiver(vm, at));
        Ok(Value::num(range.map_or(Num::NAN, |r| r.from.min(r.to))))
    });
    define(vm, class, "max", |vm, at| {
        let range = range_of(vm, receiver(vm, at));
        Ok(Value::num(range.map_or(Num::NAN, |r| r.from.max(r.to))))
    });

    // Upstream's `Range.iterate` verbatim in behaviour, including the two edge
    // cases that are easy to get wrong: an exclusive empty range, and a
    // descending range counting down.
    define(vm, class, "iterate(_)", |vm, at| {
        let Some(range) = range_of(vm, receiver(vm, at)) else {
            return Err(RuntimeError::new("Receiver must be a range."));
        };

        if range.from == range.to && !range.is_inclusive {
            return Ok(Value::FALSE);
        }

        let current = argument(vm, at, 1);
        if current.is_null() {
            return Ok(Value::num(range.from));
        }
        // **A fractional iterator is allowed**, and simply steps from where it
        // is: `for (i in 1..3)` always passes integers, but a program may call
        // `iterate` directly with anything, and upstream answers rather than
        // refusing.
        let mut iterator = argument(vm, at, 1)
            .as_num()
            .ok_or_else(|| RuntimeError::new("Iterator must be a number."))?;

        if range.from < range.to {
            iterator += 1.0;
            if iterator > range.to {
                return Ok(Value::FALSE);
            }
        } else {
            iterator -= 1.0;
            if iterator < range.to {
                return Ok(Value::FALSE);
            }
        }

        if !range.is_inclusive && iterator == range.to {
            return Ok(Value::FALSE);
        }

        Ok(Value::num(iterator))
    });

    // For a range the iterator *is* the value, which is why iterating one
    // allocates nothing at all.
    define(vm, class, "iteratorValue(_)", |vm, at| {
        Ok(argument(vm, at, 1))
    });

    define(vm, class, "isInclusive", |vm, at| {
        Ok(Value::bool(
            range_of(vm, receiver(vm, at)).is_some_and(|r| r.is_inclusive),
        ))
    });

    // **All three fields, so `2..5` and `2...5` are different ranges.** They
    // cover different values, so comparing only the endpoints would make two
    // ranges equal that iterate differently.
    define(vm, class, "==(_)", |vm, at| {
        let left = range_of(vm, receiver(vm, at));
        let right = range_of(vm, argument(vm, at, 1));
        Ok(Value::bool(match (left, right) {
            (Some(a), Some(b)) => {
                a.from == b.from && a.to == b.to && a.is_inclusive == b.is_inclusive
            }
            _ => false,
        }))
    });
    define(vm, class, "!=(_)", |vm, at| {
        let left = range_of(vm, receiver(vm, at));
        let right = range_of(vm, argument(vm, at, 1));
        Ok(Value::bool(match (left, right) {
            (Some(a), Some(b)) => {
                !(a.from == b.from && a.to == b.to && a.is_inclusive == b.is_inclusive)
            }
            _ => true,
        }))
    });

    define(vm, class, "toString", |vm, at| {
        let text = vm.to_string(receiver(vm, at));
        Ok(vm.new_string(&text))
    });
}

/// The element indices a range selects, in the order it selects them.
///
/// Handles the two things that make slicing fiddly: a descending range reads
/// backwards, and an exclusive range stops one short of its end.
fn slice_indices(range: &ObjRange, length: usize) -> Result<Vec<usize>, RuntimeError> {
    // **An empty range at the end is allowed**, and is checked against the raw
    // bounds before negative indices are resolved. This is what makes
    // `list[0..-1]` and `list[0...list.count]` copy a list that may be empty:
    // without it, a start equal to the length is out of bounds and the idiom
    // fails on exactly the case it exists for.
    let end = if range.is_inclusive {
        -1.0
    } else {
        length as Num
    };
    if range.from == length as Num && range.to == end {
        return Ok(Vec::new());
    }

    let resolve = |value: Num| -> Result<isize, RuntimeError> {
        if value != math::trunc(value) {
            return Err(RuntimeError::new("Range start must be an integer."));
        }
        let resolved = if value < 0.0 {
            value + length as Num
        } else {
            value
        };
        Ok(resolved as isize)
    };

    let from = resolve(range.from)?;
    let to = resolve(range.to)?;
    let descending = to < from;

    let last = if range.is_inclusive {
        to
    } else if descending {
        to + 1
    } else {
        to - 1
    };

    // An exclusive empty range selects nothing rather than failing.
    if !range.is_inclusive && from == to {
        return Ok(Vec::new());
    }
    if from < 0 || from >= length as isize {
        return Err(RuntimeError::new("Range start out of bounds."));
    }
    if last < 0 || last >= length as isize {
        return Err(RuntimeError::new("Range end out of bounds."));
    }

    let mut indices = Vec::new();
    if descending {
        let mut index = from;
        while index >= last {
            indices.push(index as usize);
            index -= 1;
        }
    } else {
        let mut index = from;
        while index <= last {
            indices.push(index as usize);
            index += 1;
        }
    }
    Ok(indices)
}

fn range_of(vm: &Vm, value: Value) -> Option<ObjRange> {
    vm.heap.range(value.as_object()?).copied()
}

/// `System`, and the metaclass that holds its static methods.
fn install_system(vm: &mut Vm) {
    let metaclass_name = vm
        .heap
        .allocate(Object::String(ObjString::from_text("System metaclass")));
    let metaclass = vm.heap.allocate(Object::Class(Box::new(ObjClass::new(
        metaclass_name,
        Some(vm.class_class),
    ))));

    let name = vm
        .heap
        .allocate(Object::String(ObjString::from_text("System")));
    let mut class = ObjClass::new(name, None);
    class.metaclass = Some(metaclass);
    let system = vm.heap.allocate(Object::Class(Box::new(class)));

    define(vm, metaclass, "print(_)", |vm, at| {
        let value = argument(vm, at, 1);
        let text = vm.stringify(value)?;
        vm.output.extend_from_slice(text.as_bytes());
        vm.output.push(b'\n');
        // `System.print(x)` returns x, so it can be dropped into an expression.
        Ok(value)
    });

    // Both the getter and the zero-argument method: `System.print` and
    // `System.print()` are different signatures in Wren and both print a
    // newline.
    define(vm, metaclass, "print", |vm, _| {
        vm.output.push(b'\n');
        Ok(Value::NULL)
    });
    define(vm, metaclass, "print()", |vm, _| {
        vm.output.push(b'\n');
        Ok(Value::NULL)
    });

    define(vm, metaclass, "gc()", |vm, _| {
        vm.collect_garbage();
        Ok(Value::NULL)
    });

    // `System.clock` is what every benchmark times itself with.
    define(vm, metaclass, "clock", |vm, _| match vm.clock.as_ref() {
        // The host hands back a `f64` whatever this build's `Num` is, so
        // that a port does not have to know which one it was compiled with.
        Some(clock) => Ok(Value::num(clock() as Num)),
        None => Err(RuntimeError::new("This host provides no clock.")),
    });

    define(vm, metaclass, "printAll(_)", |vm, at| {
        let sequence = argument(vm, at, 1);
        for element in collect(vm, sequence)? {
            let text = vm.stringify(element)?;
            vm.output.extend_from_slice(text.as_bytes());
        }
        vm.output.push(b'\n');
        Ok(sequence)
    });

    define(vm, metaclass, "writeAll(_)", |vm, at| {
        let sequence = argument(vm, at, 1);
        for element in collect(vm, sequence)? {
            let text = vm.stringify(element)?;
            vm.output.extend_from_slice(text.as_bytes());
        }
        Ok(sequence)
    });

    define(vm, metaclass, "write(_)", |vm, at| {
        let value = argument(vm, at, 1);
        let text = vm.stringify(value)?;
        vm.output.extend_from_slice(text.as_bytes());
        Ok(value)
    });

    vm.modules[0].define("System", Value::object(system));

    // **The core classes have to be reachable by name.** `class Foo {}` with no
    // `is` clause compiles to a load of `Object`, and `x is Num` needs `Num`.
    for (name, class) in [
        ("Object", vm.object_class),
        ("Bool", vm.bool_class),
        ("Class", vm.class_class),
        ("Fn", vm.fn_class),
        ("List", vm.list_class),
        ("Map", vm.map_class),
        ("Null", vm.null_class),
        ("Num", vm.num_class),
        ("Fiber", vm.fiber_class),
        ("Range", vm.range_class),
        ("Sequence", vm.sequence_class),
        ("MapEntry", vm.map_entry_class),
        ("MapKeySequence", vm.map_key_sequence_class),
        ("MapValueSequence", vm.map_value_sequence_class),
        ("ClassAttributes", vm.class_attributes_class),
        ("StringByteSequence", vm.string_byte_sequence_class),
        (
            "StringCodePointSequence",
            vm.string_code_point_sequence_class,
        ),
        ("String", vm.string_class),
    ] {
        vm.modules[0].define(name, Value::object(class));
    }
}

/// Give every core class a metaclass, named after it.
///
/// **A class's own class is its metaclass**, so `Object.type.name` is
/// `"Object metaclass"` rather than `"Class"`. Several core classes had none,
/// because only the ones with static methods had needed one -- which made
/// their `type` fall back to `Class` and report the wrong name.
fn install_metaclasses(vm: &mut Vm) {
    // **`Class` is deliberately absent.** Its own metatype is itself: `class_of`
    // falls back to `Class` for a class with no metaclass, so leaving it
    // without one is what makes `Class.type == Class` and the chain of
    // `.type.type.type` settle rather than growing a new metaclass each step.
    let classes = [
        vm.object_class,
        vm.bool_class,
        vm.fiber_class,
        vm.fn_class,
        vm.list_class,
        vm.map_class,
        vm.map_entry_class,
        vm.null_class,
        vm.num_class,
        vm.range_class,
        vm.string_class,
        vm.sequence_class,
        vm.map_sequence_class,
        vm.where_sequence_class,
        vm.take_sequence_class,
        vm.skip_sequence_class,
    ];
    for class in classes {
        let existing = match vm.heap.class(class) {
            Some(class) => class.metaclass,
            _ => continue,
        };
        if existing.is_some() {
            continue;
        }
        let name = match vm.heap.class(class) {
            Some(class) => class.name,
            _ => continue,
        };
        let text = match vm.heap.string(name) {
            Some(text) => text.as_str().unwrap_or("?").to_string(),
            _ => "?".to_string(),
        };
        let metaclass_name =
            vm.heap
                .allocate(Object::String(ObjString::from_text(&alloc::format!(
                    "{text} metaclass"
                ))));
        let metaclass = vm.heap.allocate(Object::Class(Box::new(ObjClass::new(
            metaclass_name,
            Some(vm.class_class),
        ))));
        if let Some(class) = vm.heap.class_mut(class) {
            class.metaclass = Some(metaclass);
        }
    }
}

/// Build the built-in `meta` module and return its index.
///
/// **Only `eval` and `getModuleVariables`**, which is what the suite asks for.
/// Upstream splits each into a Wren wrapper that validates its argument and a
/// foreign leaf that does the work; written as primitives the two collapse
/// into one each, and the wrapper's error messages are kept because they are
/// what a program sees.
pub fn install_meta(vm: &mut Vm) -> usize {
    let name = vm
        .heap
        .allocate(Object::String(ObjString::from_text("Meta")));
    let class = vm.heap.allocate(Object::Class(Box::new(ObjClass::new(
        name,
        Some(vm.object_class),
    ))));

    let metaclass_name = vm
        .heap
        .allocate(Object::String(ObjString::from_text("Meta metaclass")));
    let metaclass = vm.heap.allocate(Object::Class(Box::new(ObjClass::new(
        metaclass_name,
        Some(vm.class_class),
    ))));
    if let Some(meta) = vm.heap.class_mut(class) {
        meta.metaclass = Some(metaclass);
    }

    define(vm, metaclass, "getModuleVariables(_)", |vm, at| {
        let value = argument(vm, at, 1);
        if !vm.is_string(value) {
            return Err(RuntimeError::new("Module name must be a string."));
        }
        let name = vm.to_string(value);
        let Some(index) = vm.module_index.get(&name).copied() else {
            return Err(RuntimeError::new(alloc::format!(
                "Could not find a module named '{name}'."
            )));
        };
        // The names are collected first as owned strings: allocating the Wren
        // strings needs the VM mutably, and the symbol table is borrowed out
        // of it.
        let names: Vec<alloc::string::String> = (0..vm.modules[index].names.len())
            .filter_map(|slot| vm.modules[index].names.name(slot).map(ToString::to_string))
            .collect();
        let elements = names.iter().map(|name| vm.new_string(name)).collect();
        Ok(new_list(vm, elements))
    });

    // `Meta.eval` compiles, so it exists only where a compiler does.
    #[cfg(feature = "compiler")]
    define(vm, metaclass, "eval(_)", |vm, at| {
        let value = argument(vm, at, 1);
        if !vm.is_string(value) {
            return Err(RuntimeError::new("Source code must be a string."));
        }
        let source = vm.to_string(value);

        // **Compiled into the caller's module**, so `y = 2` assigns the `y`
        // the caller declared rather than one in a namespace of its own. A
        // primitive pushes no frame, so the top frame is still the caller's.
        let module = vm.current_module();
        let chunk = match crate::compiler::compile_in(vm, &source, module) {
            Ok(chunk) => chunk,
            Err(error) => {
                return Err(RuntimeError::new(alloc::format!(
                    "Could not compile source code: {}",
                    error.message
                )))
            }
        };

        let function = vm.heap.allocate(Object::Fn(Box::new(crate::object::ObjFn {
            chunk: alloc::rc::Rc::new(chunk),
            arity: 0,
            num_upvalues: 0,
            name: "(eval)".into(),
            field_offset: 0,
            super_class: None,
            owner_class: None,
            module,
        })));
        let closure = vm
            .heap
            .allocate(Object::Closure(Box::new(crate::object::ObjClosure {
                function,
                upvalues: Vec::new(),
            })));

        let base = vm.stack.len();
        vm.stack.push(Value::object(closure));
        let outcome = vm.call_closure(closure, base);
        vm.stack.truncate(base);
        outcome?;
        Ok(Value::NULL)
    });

    let mut module = crate::vm::Module::new();
    module.name = alloc::string::String::from("meta");
    module.define("Meta", Value::object(class));
    vm.modules.push(module);
    let index = vm.modules.len() - 1;
    vm.module_index
        .insert(alloc::string::String::from("meta"), index);
    index
}

/// Build the built-in `random` module and return its index.
///
/// **A native module rather than Wren source.** Upstream ships `random` as a
/// `.wren` file compiled at start-up; compiling it on a part with 8 KB is the
/// thing this implementation is trying not to do, so the class is built
/// directly.
///
/// The generator is xorshift128, not upstream's WELL512a. Upstream's tests ask
/// for statistical properties -- "roughly evenly distributed", "within range"
/// -- and never for a particular sequence, so matching the exact algorithm
/// would buy nothing. If a program ever needs to reproduce upstream's stream
/// from a seed, that is the point at which WELL512a becomes worth writing.
pub fn install_random(vm: &mut Vm) -> usize {
    let name = vm
        .heap
        .allocate(Object::String(ObjString::from_text("Random")));
    let class = vm.heap.allocate(Object::Class(Box::new(ObjClass::new(
        name,
        Some(vm.object_class),
    ))));

    let metaclass_name = vm
        .heap
        .allocate(Object::String(ObjString::from_text("Random metaclass")));
    let metaclass = vm.heap.allocate(Object::Class(Box::new(ObjClass::new(
        metaclass_name,
        Some(vm.class_class),
    ))));
    if let Some(random) = vm.heap.class_mut(class) {
        random.metaclass = Some(metaclass);
        // **Eight fields, not four.** The generator's state is four `u32`
        // words, and a `Num` only holds one exactly when it is a double: an
        // `f32` has a 24-bit mantissa, so storing a word per field would
        // silently round the state and collapse the sequence. Each word is
        // kept as two 16-bit halves, which both widths hold exactly.
        random.num_fields = 8;
    }

    define(vm, metaclass, "new()", |vm, _| {
        let class = vm.random_class;
        let seed = default_seed();
        Ok(new_random(vm, class, seed))
    });
    define(vm, metaclass, "new(_)", |vm, at| {
        let argument = argument(vm, at, 1);
        // **A sequence seeds from its elements**, so two generators built from
        // equal sequences produce the same stream -- which is what the seed is
        // for, and a sequence is a perfectly good way to spell one.
        let seed = match argument.as_num() {
            Some(number) => number as i64 as u32,
            None => {
                let elements = collect(vm, argument)?;
                if elements.is_empty() {
                    return Err(RuntimeError::new("Sequence cannot be empty."));
                }
                let mut seed: u32 = 0;
                for element in elements {
                    // **Numbers only.** A seed built from arbitrary objects
                    // would depend on where they happen to sit in the heap,
                    // so two equal sequences would seed differently -- which
                    // defeats the point of seeding from one.
                    let Some(number) = element.as_num() else {
                        return Err(RuntimeError::new("Sequence elements must all be numbers."));
                    };
                    seed = seed.wrapping_mul(31).wrapping_add(number as i64 as u32);
                }
                seed
            }
        };
        let class = vm.random_class;
        Ok(new_random(vm, class, seed))
    });

    define(vm, class, "float()", |vm, at| {
        let value = next_float(vm, receiver(vm, at));
        Ok(Value::num(value as Num))
    });
    define(vm, class, "float(_)", |vm, at| {
        let end = number_argument(vm, at, 1)?;
        let value = next_float(vm, receiver(vm, at));
        Ok(Value::num(value as Num * end))
    });
    define(vm, class, "float(_,_)", |vm, at| {
        let start = number_argument(vm, at, 1)?;
        let end = number_argument(vm, at, 2)?;
        let value = next_float(vm, receiver(vm, at));
        Ok(Value::num(start + value as Num * (end - start)))
    });

    define(vm, class, "int()", |vm, at| {
        let value = next_u32(vm, receiver(vm, at));
        Ok(Value::num(value as Num))
    });
    define(vm, class, "int(_)", |vm, at| {
        let end = number_argument(vm, at, 1)?;
        let value = next_float(vm, receiver(vm, at));
        Ok(Value::num(math::floor(value as Num * end)))
    });
    define(vm, class, "int(_,_)", |vm, at| {
        let start = number_argument(vm, at, 1)?;
        let end = number_argument(vm, at, 2)?;
        let value = next_float(vm, receiver(vm, at));
        Ok(Value::num(
            start + math::floor(value as Num * (end - start)),
        ))
    });

    define(vm, class, "sample(_)", |vm, at| {
        let list = argument(vm, at, 1);
        let elements = list_elements(vm, list);
        if elements.is_empty() {
            return Err(RuntimeError::new("Not enough elements to sample."));
        }
        let pick = next_float(vm, receiver(vm, at));
        let index = (pick * elements.len() as f64) as usize;
        Ok(elements[index.min(elements.len() - 1)])
    });

    define(vm, class, "sample(_,_)", |vm, at| {
        let list = argument(vm, at, 1);
        let count = number_argument(vm, at, 2)?;
        let mut elements = list_elements(vm, list);
        if count < 0.0 || count as usize > elements.len() {
            return Err(RuntimeError::new("Not enough elements to sample."));
        }
        // **Without replacement**, which is what Wren's `sample` promises: a
        // partial Fisher-Yates, taking the first `count` after shuffling only
        // as far as needed.
        let wanted = count as usize;
        let receiver = receiver(vm, at);
        for index in 0..wanted {
            let pick = next_float(vm, receiver);
            let last = elements.len() - 1;
            let choice = index + (pick * (elements.len() - index) as f64) as usize;
            elements.swap(index, choice.min(last));
        }
        elements.truncate(wanted);
        Ok(new_list(vm, elements))
    });

    define(vm, class, "shuffle(_)", |vm, at| {
        let list = argument(vm, at, 1);
        let mut elements = list_elements(vm, list);
        if elements.is_empty() {
            return Ok(list);
        }
        let receiver = receiver(vm, at);
        // Fisher-Yates, downward, which is the version that needs no rejection
        // and touches each element once.
        let mut index = elements.len() - 1;
        while index > 0 {
            let pick = next_float(vm, receiver);
            let choice = (pick * (index + 1) as f64) as usize;
            elements.swap(index, choice.min(index));
            index -= 1;
        }
        if let Some(target) = list.as_object().and_then(|id| vm.heap.list_mut(id)) {
            target.elements = elements;
        }
        Ok(list)
    });

    vm.random_class = class;

    let mut module = crate::vm::Module::new();
    module.define("Random", Value::object(class));
    vm.modules.push(module);
    let index = vm.modules.len() - 1;
    vm.module_index
        .insert(alloc::string::String::from("random"), index);
    index
}

fn new_random(vm: &mut Vm, class: crate::handle::ObjectId, seed: u32) -> Value {
    // Spread one seed word across four state words. A state of all zeros is
    // the one xorshift cannot escape, so the constants guarantee it never is.
    let mut state = [
        seed ^ 0x9e37_79b9,
        seed.wrapping_mul(0x85eb_ca6b) | 1,
        seed.wrapping_mul(0xc2b2_ae35) ^ 0x1234_5678,
        seed.rotate_left(16) | 0x8000_0000,
    ];
    // Discard the first few outputs, which are the most correlated with the
    // seed.
    for _ in 0..8 {
        step(&mut state);
    }
    let fields = state.iter().flat_map(|word| halves(*word)).collect();
    let id = vm
        .heap
        .allocate(Object::Instance(ObjInstance { class, fields }));
    Value::object(id)
}

/// Split a state word into the two 16-bit numbers it is stored as.
fn halves(word: u32) -> [Value; 2] {
    [
        Value::num((word & 0xffff) as Num),
        Value::num((word >> 16) as Num),
    ]
}

/// Read one 16-bit half back out of an instance's fields.
fn field_half(instance: &ObjInstance, slot: usize) -> u32 {
    match instance
        .fields
        .get(slot)
        .copied()
        .and_then(|value| value.as_num())
    {
        Some(half) => half as u32 & 0xffff,
        None => 0,
    }
}

/// xorshift128, one step.
fn step(state: &mut [u32; 4]) -> u32 {
    let mut t = state[0];
    t ^= t << 11;
    t ^= t >> 8;
    state[0] = state[1];
    state[1] = state[2];
    state[2] = state[3];
    state[3] = state[3] ^ (state[3] >> 19) ^ t;
    state[3]
}

fn next_u32(vm: &mut Vm, receiver: Value) -> u32 {
    let Some(id) = receiver.as_object() else {
        return 0;
    };
    let mut state = [0u32; 4];
    match vm.heap.instance(id) {
        Some(instance) => {
            for (word, slot) in state.iter_mut().zip((0..8).step_by(2)) {
                let low = field_half(instance, slot);
                let high = field_half(instance, slot + 1);
                *word = low | (high << 16);
            }
        }
        _ => return 0,
    }
    let value = step(&mut state);
    if let Some(instance) = vm.heap.instance_mut(id) {
        for (word, slot) in state.iter().zip((0..8).step_by(2)) {
            let [low, high] = halves(*word);
            instance.fields[slot] = low;
            instance.fields[slot + 1] = high;
        }
    }
    value
}

/// A double in `[0, 1)`, built from 53 bits so every representable value in
/// the range is reachable -- 32 bits would leave most of them unused.
///
/// Computed at full width and narrowed by the caller, so a 32-bit build draws
/// from the same stream rather than from a differently-quantised one.
fn next_float(vm: &mut Vm, receiver: Value) -> f64 {
    let high = next_u32(vm, receiver) as u64;
    let low = next_u32(vm, receiver) as u64;
    let bits = ((high << 21) ^ (low >> 11)) & ((1u64 << 53) - 1);
    bits as f64 / (1u64 << 53) as f64
}

#[cfg(feature = "std")]
fn default_seed() -> u32 {
    // Entropy from the clock. Not cryptographic, and not claimed to be -- it
    // exists so two runs of the same program differ.
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.subsec_nanos() ^ since.as_secs() as u32)
        .unwrap_or(0x1234_5678)
}

#[cfg(not(feature = "std"))]
fn default_seed() -> u32 {
    // **No clock on a bare-metal target**, so an unseeded generator repeats
    // across resets. Saying so is better than inventing entropy that is not
    // there; a firmware that needs a varying stream should seed from something
    // it actually has, such as an ADC reading.
    0x1234_5678
}

/// A fresh empty map, for the compiler building an attribute table.
pub fn new_map(vm: &mut Vm) -> Value {
    Value::object(vm.heap.allocate(Object::Map(ObjMap::new())))
}

/// Put a key and value into a map, for the same.
pub fn map_insert(vm: &mut Vm, map: Value, key: Value, value: Value) {
    let _ = map_set(vm, map, key, value);
}

/// Read a key back out, so the compiler can accumulate into nested maps.
pub fn map_lookup(vm: &Vm, map: Value, key: Value) -> Option<Value> {
    map_get(vm, map, key)
}

/// Append to a list, for accumulating repeated attribute keys.
pub fn list_push(vm: &mut Vm, list: Value, value: Value) {
    if let Some(list) = list.as_object().and_then(|id| vm.heap.list_mut(id)) {
        list.elements.push(value);
    }
}

/// Build a list value from elements, for the compiler's list literals.
pub fn new_list(vm: &mut Vm, elements: Vec<Value>) -> Value {
    let id = vm.heap.allocate(Object::List(ObjList { elements }));
    Value::object(id)
}
