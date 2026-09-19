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

extern crate alloc;

use alloc::boxed::Box;

use alloc::vec::Vec;

use crate::handle::ObjectId;
use crate::math;
use crate::object::{Method, ObjClass, ObjList, ObjRange, ObjString, Object, Primitive};
use crate::value::Value;
use crate::vm::{RuntimeError, Vm};

/// Bind a primitive to a signature on a class.
fn define(vm: &mut Vm, class: ObjectId, signature: &str, function: Primitive) {
    let symbol = vm.method_names.ensure(signature);
    if let Some(Object::Class(class)) = vm.heap.get_mut(class) {
        class.define(symbol, Method::Primitive(function));
    }
}

/// The receiver of the call in progress.
fn receiver(vm: &Vm, at: usize) -> Value {
    vm.stack[at]
}

/// Argument `index`, counting from one as Wren's own `args[1]` does.
fn argument(vm: &Vm, at: usize, index: usize) -> Value {
    vm.stack[at + index]
}

fn number_argument(vm: &Vm, at: usize, index: usize) -> Result<f64, RuntimeError> {
    argument(vm, at, index)
        .as_num()
        .ok_or_else(|| RuntimeError::new("Right operand must be a number."))
}

/// Define a binary arithmetic operator on `Num`.
macro_rules! arithmetic {
    ($vm:expr, $class:expr, $signature:literal, $left:ident, $right:ident, $body:expr) => {
        define($vm, $class, $signature, |vm, at| {
            let $left = receiver(vm, at).as_num().unwrap_or(f64::NAN);
            let $right = number_argument(vm, at, 1)?;
            Ok($body)
        });
    };
}

/// Install the core library into a fresh VM.
pub fn install(vm: &mut Vm) {
    install_object(vm);
    install_num(vm);
    install_bool(vm);
    install_null(vm);
    install_string(vm);
    install_list(vm);
    install_range(vm);
    install_system(vm);
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
        let from = receiver(vm, at).as_num().unwrap_or(f64::NAN);
        let to = number_argument(vm, at, 1)?;
        let id = vm.heap.allocate(Object::Range(ObjRange { from, to, is_inclusive: true }));
        Ok(Value::object(id))
    });
    define(vm, class, "...(_)", |vm, at| {
        let from = receiver(vm, at).as_num().unwrap_or(f64::NAN);
        let to = number_argument(vm, at, 1)?;
        let id = vm.heap.allocate(Object::Range(ObjRange { from, to, is_inclusive: false }));
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
        Ok(Value::num(-receiver(vm, at).as_num().unwrap_or(f64::NAN)))
    });
    define(vm, class, "abs", |vm, at| {
        Ok(Value::num(math::abs(receiver(vm, at).as_num().unwrap_or(f64::NAN))))
    });
    define(vm, class, "floor", |vm, at| {
        Ok(Value::num(math::floor(receiver(vm, at).as_num().unwrap_or(f64::NAN))))
    });
    define(vm, class, "ceil", |vm, at| {
        Ok(Value::num(math::ceil(receiver(vm, at).as_num().unwrap_or(f64::NAN))))
    });
    define(vm, class, "sqrt", |vm, at| {
        Ok(Value::num(math::sqrt(receiver(vm, at).as_num().unwrap_or(f64::NAN))))
    });
    define(vm, class, "toString", |vm, at| {
        let text = vm.to_string(receiver(vm, at));
        Ok(vm.new_string(&text))
    });
}

fn install_bool(vm: &mut Vm) {
    let class = vm.bool_class;
    define(vm, class, "!", |vm, at| Ok(Value::bool(receiver(vm, at).is_falsy())));
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
        if vm.string_at(right).is_none() {
            return Err(RuntimeError::new("Right operand must be a string."));
        }
        let joined = alloc::format!("{}{}", left, vm.to_string(right));
        Ok(vm.new_string(&joined))
    });

    define(vm, class, "count", |vm, at| {
        // Bytes, as upstream's `count` is. A string's length in *characters*
        // needs decoding and is a different method there.
        let text = receiver(vm, at);
        let length = match vm.heap.get(text.as_object().unwrap()) {
            Some(Object::String(string)) => string.bytes.len(),
            _ => 0,
        };
        Ok(Value::num(length as f64))
    });

    define(vm, class, "toString", |vm, at| Ok(receiver(vm, at)));

    define(vm, class, "==(_)", |vm, at| {
        Ok(Value::bool(strings_equal(vm, receiver(vm, at), argument(vm, at, 1))))
    });
    define(vm, class, "!=(_)", |vm, at| {
        Ok(Value::bool(!strings_equal(vm, receiver(vm, at), argument(vm, at, 1))))
    });
}

/// Wren compares strings by **contents**, not by identity.
fn strings_equal(vm: &Vm, left: Value, right: Value) -> bool {
    let (Some(left), Some(right)) = (left.as_object(), right.as_object()) else {
        return false;
    };
    match (vm.heap.get(left), vm.heap.get(right)) {
        (Some(Object::String(a)), Some(Object::String(b))) => {
            // The cached hash is a cheap rejection before comparing bytes.
            a.hash() == b.hash() && a.bytes == b.bytes
        }
        _ => false,
    }
}

fn install_list(vm: &mut Vm) {
    let class = vm.list_class;

    // `List` has to be reachable by name, because a list literal compiles to
    // `List.new` followed by an `addCore(_)` per element rather than to an
    // opcode of its own.
    let metaclass_name = vm.heap.allocate(Object::String(ObjString::from_text("List metaclass")));
    let metaclass = vm
        .heap
        .allocate(Object::Class(Box::new(ObjClass::new(metaclass_name, None))));
    if let Some(Object::Class(list)) = vm.heap.get_mut(class) {
        list.metaclass = Some(metaclass);
    }
    define(vm, metaclass, "new", |vm, _| {
        Ok(new_list(vm, Vec::new()))
    });
    vm.module.define("List", Value::object(class));

    // Like `add(_)`, but returns the *list* rather than the element, so a list
    // literal can add each element without reloading the list between them.
    define(vm, class, "addCore(_)", |vm, at| {
        let list = receiver(vm, at);
        let element = argument(vm, at, 1);
        match vm.heap.get_mut(list.as_object().unwrap()) {
            Some(Object::List(list)) => list.elements.push(element),
            _ => return Err(RuntimeError::new("Receiver must be a list.")),
        }
        Ok(list)
    });

    define(vm, class, "add(_)", |vm, at| {
        let list = receiver(vm, at);
        let element = argument(vm, at, 1);
        match vm.heap.get_mut(list.as_object().unwrap()) {
            Some(Object::List(list)) => list.elements.push(element),
            _ => return Err(RuntimeError::new("Receiver must be a list.")),
        }
        // `add` returns the element, which is what makes `list.add(x)` usable
        // as an expression.
        Ok(element)
    });

    define(vm, class, "count", |vm, at| {
        Ok(Value::num(list_length(vm, receiver(vm, at)) as f64))
    });

    define(vm, class, "[_]", |vm, at| {
        let list = receiver(vm, at);
        let index = number_argument(vm, at, 1)?;
        let length = list_length(vm, list);
        let index = resolve_index(index, length)?;
        match vm.heap.get(list.as_object().unwrap()) {
            Some(Object::List(list)) => Ok(list.elements[index]),
            _ => Err(RuntimeError::new("Receiver must be a list.")),
        }
    });

    define(vm, class, "[_]=(_)", |vm, at| {
        let list = receiver(vm, at);
        let index = number_argument(vm, at, 1)?;
        let value = argument(vm, at, 2);
        let length = list_length(vm, list);
        let index = resolve_index(index, length)?;
        match vm.heap.get_mut(list.as_object().unwrap()) {
            Some(Object::List(list)) => {
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
        let index = current
            .as_num()
            .ok_or_else(|| RuntimeError::new("Iterator must be a number."))?;
        if index < 0.0 || index >= (length - 1) as f64 {
            return Ok(Value::FALSE);
        }
        Ok(Value::num(index + 1.0))
    });

    define(vm, class, "iteratorValue(_)", |vm, at| {
        let list = receiver(vm, at);
        let index = number_argument(vm, at, 1)?;
        let length = list_length(vm, list);
        let index = resolve_index(index, length)?;
        match vm.heap.get(list.as_object().unwrap()) {
            Some(Object::List(list)) => Ok(list.elements[index]),
            _ => Err(RuntimeError::new("Receiver must be a list.")),
        }
    });

    define(vm, class, "toString", |vm, at| {
        let text = vm.to_string(receiver(vm, at));
        Ok(vm.new_string(&text))
    });
}

fn list_length(vm: &Vm, list: Value) -> usize {
    match list.as_object().and_then(|id| vm.heap.get(id)) {
        Some(Object::List(list)) => list.elements.len(),
        _ => 0,
    }
}

/// Turn a possibly-negative index into a real one, as Wren does.
///
/// `list[-1]` is the last element. Upstream's message for an out-of-range index
/// is exactly this, and the suite checks it.
fn resolve_index(index: f64, length: usize) -> Result<usize, RuntimeError> {
    if index != math::trunc(index) {
        return Err(RuntimeError::new("Index must be an integer."));
    }
    let resolved = if index < 0.0 { index + length as f64 } else { index };
    if resolved < 0.0 || resolved >= length as f64 {
        return Err(RuntimeError::new("Index out of bounds."));
    }
    Ok(resolved as usize)
}

fn install_range(vm: &mut Vm) {
    let class = vm.range_class;

    define(vm, class, "from", |vm, at| {
        Ok(Value::num(range_of(vm, receiver(vm, at)).map_or(f64::NAN, |r| r.from)))
    });
    define(vm, class, "to", |vm, at| {
        Ok(Value::num(range_of(vm, receiver(vm, at)).map_or(f64::NAN, |r| r.to)))
    });
    define(vm, class, "min", |vm, at| {
        let range = range_of(vm, receiver(vm, at));
        Ok(Value::num(range.map_or(f64::NAN, |r| r.from.min(r.to))))
    });
    define(vm, class, "max", |vm, at| {
        let range = range_of(vm, receiver(vm, at));
        Ok(Value::num(range.map_or(f64::NAN, |r| r.from.max(r.to))))
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
        let mut iterator = current
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
    define(vm, class, "iteratorValue(_)", |vm, at| Ok(argument(vm, at, 1)));

    define(vm, class, "toString", |vm, at| {
        let text = vm.to_string(receiver(vm, at));
        Ok(vm.new_string(&text))
    });
}

fn range_of(vm: &Vm, value: Value) -> Option<ObjRange> {
    match vm.heap.get(value.as_object()?)? {
        Object::Range(range) => Some(*range),
        _ => None,
    }
}

/// `System`, and the metaclass that holds its static methods.
fn install_system(vm: &mut Vm) {
    let metaclass_name = vm.heap.allocate(Object::String(ObjString::from_text("System metaclass")));
    let metaclass = vm
        .heap
        .allocate(Object::Class(Box::new(ObjClass::new(metaclass_name, None))));

    let name = vm.heap.allocate(Object::String(ObjString::from_text("System")));
    let mut class = ObjClass::new(name, None);
    class.metaclass = Some(metaclass);
    let system = vm.heap.allocate(Object::Class(Box::new(class)));

    define(vm, metaclass, "print(_)", |vm, at| {
        let value = argument(vm, at, 1);
        let text = vm.to_string(value);
        vm.output.extend_from_slice(text.as_bytes());
        vm.output.push(b'\n');
        // `System.print(x)` returns x, so it can be dropped into an expression.
        Ok(value)
    });

    define(vm, metaclass, "print", |vm, _| {
        vm.output.push(b'\n');
        Ok(Value::NULL)
    });

    define(vm, metaclass, "write(_)", |vm, at| {
        let value = argument(vm, at, 1);
        let text = vm.to_string(value);
        vm.output.extend_from_slice(text.as_bytes());
        Ok(value)
    });

    vm.module.define("System", Value::object(system));
}

/// Build a list value from elements, for the compiler's list literals.
pub fn new_list(vm: &mut Vm, elements: Vec<Value>) -> Value {
    let id = vm.heap.allocate(Object::List(ObjList { elements }));
    Value::object(id)
}
