//! End to end: source in, output out.
//!
//! These are written against **Wren's behaviour**, not against this
//! implementation's — where the two could differ, the expectation is whatever
//! upstream does, so that a passing test means compatibility rather than
//! self-consistency.

use wren::Vm;

/// Run a program and return what it printed.
fn run(source: &str) -> String {
    let mut vm = Vm::new();
    match vm.interpret(source) {
        Ok(()) => vm.output_str().to_string(),
        Err(error) => panic!("line {}: {}", error.line(), error.message()),
    }
}

/// Run a program expected to fail, and return the message.
fn error(source: &str) -> String {
    let mut vm = Vm::new();
    match vm.interpret(source) {
        Ok(()) => panic!("expected an error, got output {:?}", vm.output_str()),
        Err(error) => error.message().to_string(),
    }
}

// --- arithmetic -------------------------------------------------------------

#[test]
fn arithmetic_and_precedence() {
    assert_eq!(run("System.print(1 + 2)"), "3\n");
    assert_eq!(run("System.print(2 + 3 * 4)"), "14\n");
    assert_eq!(run("System.print((2 + 3) * 4)"), "20\n");
    assert_eq!(run("System.print(10 - 4 - 3)"), "3\n", "minus is left-associative");
    assert_eq!(run("System.print(10 / 4)"), "2.5\n");
    assert_eq!(run("System.print(10 % 3)"), "1\n");
}

#[test]
fn modulo_takes_the_sign_of_the_dividend() {
    // C's fmod, not Rust's rem_euclid. `-7 % 3` is -1 in Wren, not 2.
    assert_eq!(run("System.print(-7 % 3)"), "-1\n");
}

#[test]
fn integral_numbers_print_without_a_decimal_point() {
    // The single most visible formatting rule in the language: Wren has one
    // numeric type, but `1` prints as `1` rather than `1.0`.
    assert_eq!(run("System.print(1)"), "1\n");
    assert_eq!(run("System.print(1.0)"), "1\n");
    assert_eq!(run("System.print(1.5)"), "1.5\n");
    assert_eq!(run("System.print(0 - 0)"), "0\n");
}

#[test]
fn unary_minus_binds_looser_than_a_call() {
    // `-5.abs` is `-(5.abs)`, which is -5. Getting this backwards is the
    // classic precedence bug and it looks like an `abs` that does not work.
    assert_eq!(run("System.print(-5.abs)"), "-5\n");
    assert_eq!(run("System.print((-5).abs)"), "5\n");
}

#[test]
fn number_methods() {
    assert_eq!(run("System.print(2.5.floor)"), "2\n");
    assert_eq!(run("System.print(2.5.ceil)"), "3\n");
    assert_eq!(run("System.print(9.sqrt)"), "3\n");
    assert_eq!(run("System.print(3.toString)"), "3\n");
}

// --- comparison and equality ------------------------------------------------

#[test]
fn comparison() {
    assert_eq!(run("System.print(1 < 2)"), "true\n");
    assert_eq!(run("System.print(2 <= 2)"), "true\n");
    assert_eq!(run("System.print(3 > 4)"), "false\n");
    assert_eq!(run("System.print(3 >= 4)"), "false\n");
}

#[test]
fn equality_across_types_is_false_not_an_error() {
    assert_eq!(run("System.print(1 == \"1\")"), "false\n");
    assert_eq!(run("System.print(1 != \"1\")"), "true\n");
    assert_eq!(run("System.print(null == false)"), "false\n");
}

#[test]
fn strings_compare_by_contents() {
    // Two separately allocated strings with the same bytes are equal.
    assert_eq!(run("System.print(\"abc\" == \"abc\")"), "true\n");
    assert_eq!(run("System.print(\"abc\" == \"abd\")"), "false\n");
}

#[test]
fn only_false_and_null_are_falsy() {
    assert_eq!(run("System.print(!false)"), "true\n");
    assert_eq!(run("System.print(!null)"), "true\n");
    assert_eq!(run("System.print(!0)"), "false\n", "zero is truthy in Wren");
    assert_eq!(run("System.print(!\"\")"), "false\n", "the empty string is truthy");
}

// --- logical operators ------------------------------------------------------

#[test]
fn logical_operators_short_circuit() {
    // Proved by side effect, not by the result: if the right side ran, the
    // list would not be empty.
    let source = "
var log = []
var a = false && log.add(1)
var b = true || log.add(2)
System.print(log.count)
";
    assert_eq!(run(source), "0\n");
}

#[test]
fn logical_operators_return_a_value_not_a_boolean() {
    // Wren's `&&` yields the operand, as in Lua or Python, not `true`/`false`.
    assert_eq!(run("System.print(1 && 2)"), "2\n");
    assert_eq!(run("System.print(false && 2)"), "false\n");
    assert_eq!(run("System.print(1 || 2)"), "1\n");
    assert_eq!(run("System.print(null || 3)"), "3\n");
}

// --- variables --------------------------------------------------------------

#[test]
fn module_variables() {
    assert_eq!(run("var a = 1\nvar b = 2\nSystem.print(a + b)"), "3\n");
}

#[test]
fn a_variable_can_be_reassigned() {
    assert_eq!(run("var a = 1\na = a + 10\nSystem.print(a)"), "11\n");
}

#[test]
fn a_variable_without_an_initialiser_is_null() {
    assert_eq!(run("var a\nSystem.print(a)"), "null\n");
}

#[test]
fn locals_are_scoped_to_their_block() {
    let source = "
var a = \"outer\"
{
  var a = \"inner\"
  System.print(a)
}
System.print(a)
";
    assert_eq!(run(source), "inner\nouter\n");
}

#[test]
fn an_undefined_variable_is_a_compile_error() {
    assert_eq!(error("System.print(nope)"), "Variable is not defined.");
}

// --- control flow -----------------------------------------------------------

#[test]
fn if_without_else() {
    assert_eq!(run("if (true) System.print(\"yes\")"), "yes\n");
    assert_eq!(run("if (false) System.print(\"yes\")"), "");
}

#[test]
fn if_else_on_one_line() {
    assert_eq!(run("if (3 > 2) System.print(\"big\") else System.print(\"small\")"), "big\n");
    assert_eq!(run("if (1 > 2) System.print(\"big\") else System.print(\"small\")"), "small\n");
}

#[test]
fn if_else_with_blocks() {
    let source = "
if (false) {
  System.print(\"no\")
} else {
  System.print(\"yes\")
}
";
    assert_eq!(run(source), "yes\n");
}

#[test]
fn while_loop() {
    let source = "
var i = 0
while (i < 3) {
  System.print(i)
  i = i + 1
}
";
    assert_eq!(run(source), "0\n1\n2\n");
}

#[test]
fn while_that_never_runs() {
    assert_eq!(run("while (false) System.print(\"no\")"), "");
}

// --- iteration --------------------------------------------------------------

#[test]
fn for_over_an_inclusive_range() {
    assert_eq!(run("for (i in 1..3) System.print(i)"), "1\n2\n3\n");
}

#[test]
fn for_over_an_exclusive_range() {
    assert_eq!(run("for (i in 1...3) System.print(i)"), "1\n2\n");
}

#[test]
fn for_over_a_descending_range() {
    assert_eq!(run("for (i in 3..1) System.print(i)"), "3\n2\n1\n");
}

#[test]
fn an_empty_exclusive_range_iterates_nothing() {
    assert_eq!(run("for (i in 1...1) System.print(i)"), "");
}

#[test]
fn for_over_a_list() {
    assert_eq!(run("for (x in [10, 20]) System.print(x)"), "10\n20\n");
}

#[test]
fn for_over_an_empty_list() {
    assert_eq!(run("for (x in []) System.print(x)"), "");
}

#[test]
fn nested_loops() {
    let source = "
for (i in 1..2) {
  for (j in 1..2) {
    System.print(i * 10 + j)
  }
}
";
    assert_eq!(run(source), "11\n12\n21\n22\n");
}

#[test]
fn a_loop_accumulating_into_a_variable() {
    assert_eq!(run("var t = 0\nfor (i in 1..10) t = t + i\nSystem.print(t)"), "55\n");
}

// --- lists ------------------------------------------------------------------

#[test]
fn list_literal_and_indexing() {
    assert_eq!(run("var l = [1, 2, 3]\nSystem.print(l[0])"), "1\n");
    assert_eq!(run("var l = [1, 2, 3]\nSystem.print(l[-1])"), "3\n", "negative indexes count back");
}

#[test]
fn list_index_assignment() {
    assert_eq!(run("var l = [1, 2]\nl[0] = 9\nSystem.print(l[0])"), "9\n");
}

#[test]
fn list_add_and_count() {
    assert_eq!(run("var l = []\nl.add(1)\nl.add(2)\nSystem.print(l.count)"), "2\n");
}

#[test]
fn a_list_prints_its_elements() {
    assert_eq!(run("System.print([1, \"two\", null])"), "[1, two, null]\n");
}

#[test]
fn an_out_of_range_index_is_a_runtime_error() {
    assert_eq!(error("var l = [1]\nSystem.print(l[5])"), "Index out of bounds.");
}

// --- strings ----------------------------------------------------------------

#[test]
fn string_concatenation() {
    assert_eq!(run("System.print(\"a\" + \"b\")"), "ab\n");
}

#[test]
fn concatenating_a_non_string_is_an_error() {
    // Wren does not coerce. `"a" + 1` is an error, and quietly making it work
    // would be a different language.
    assert_eq!(error("System.print(\"a\" + 1)"), "Right operand must be a string.");
}

#[test]
fn string_escapes() {
    assert_eq!(run("System.print(\"a\\nb\")"), "a\nb\n");
    assert_eq!(run("System.print(\"a\\tb\")"), "a\tb\n");
    assert_eq!(run("System.print(\"say \\\"hi\\\"\")"), "say \"hi\"\n");
}

#[test]
fn string_count_is_in_bytes() {
    assert_eq!(run("System.print(\"abc\".count)"), "3\n");
}

#[test]
fn interpolation() {
    assert_eq!(run("var n = 7\nSystem.print(\"n is %(n)\")"), "n is 7\n");
}

#[test]
fn several_interpolations_in_one_string() {
    let source = "var n = 7\nSystem.print(\"%(n) and %(n * 2) and %(n + 1)\")";
    assert_eq!(run(source), "7 and 14 and 8\n");
}

#[test]
fn interpolation_of_a_non_string() {
    assert_eq!(run("System.print(\"list: %([1, 2])\")"), "list: [1, 2]\n");
}

// --- ranges -----------------------------------------------------------------

#[test]
fn range_properties() {
    assert_eq!(run("System.print((1..5).from)"), "1\n");
    assert_eq!(run("System.print((1..5).to)"), "5\n");
    assert_eq!(run("System.print((5..1).min)"), "1\n");
    assert_eq!(run("System.print((5..1).max)"), "5\n");
}

#[test]
fn a_range_prints_with_its_operator() {
    assert_eq!(run("System.print(1..5)"), "1..5\n");
    assert_eq!(run("System.print(1...5)"), "1...5\n");
}

// --- errors -----------------------------------------------------------------

#[test]
fn an_unknown_method_names_the_class_and_the_signature() {
    assert_eq!(error("System.print(1.nope)"), "Num does not implement 'nope'.");
}

#[test]
fn a_runtime_error_reports_its_line() {
    let mut vm = Vm::new();
    let result = vm.interpret("var a = 1\nvar b = 2\nSystem.print(a.nope)");
    assert_eq!(result.unwrap_err().line(), 3);
}

#[test]
fn wrong_operand_type() {
    assert_eq!(error("System.print(1 + \"a\")"), "Right operand must be a number.");
}

// --- the collector, under a running program ---------------------------------

#[test]
fn a_program_that_allocates_heavily_still_finishes() {
    // Every iteration builds a list and a string that become garbage
    // immediately. Nothing here checks the collector directly — the point is
    // that the VM survives, and that what is still reachable is still correct.
    let source = "
var total = 0
for (i in 1..200) {
  var scratch = [i, i + 1, i + 2]
  var text = \"item %(i)\"
  total = total + scratch[0] + text.count
}
System.print(total)
";
    // 1..200 sums to 20100; each "item N" is 5 + the digits of N.
    // 9 * 6 + 90 * 7 + 101 * 8 = 54 + 630 + 808 = 1492.
    assert_eq!(run(source), "21592\n");
}

#[test]
fn deeply_nested_expressions() {
    assert_eq!(run("System.print(((((1 + 2) * 3) - 4) * 5) + 6)"), "31\n");
}

// --- functions and classes --------------------------------------------------

#[test]
fn a_function_literal_can_be_called() {
    assert_eq!(run("var f = Fn.new { |a, b| a + b }\nSystem.print(f.call(1, 2))"), "3\n");
}

#[test]
fn a_closure_sees_later_writes_to_what_it_captured() {
    // Capturing takes the variable, not a copy of its value at the time.
    assert_eq!(run("var n = 1\nvar f = Fn.new { n }\nn = 2\nSystem.print(f.call())"), "2\n");
}

#[test]
fn two_closures_share_one_captured_variable() {
    // The case that decides whether capture reuses an upvalue or makes a new
    // one per closure: if each had its own, `read` would still say 0.
    let source = "
var n = 0
var bump = Fn.new { n = n + 1 }
var read = Fn.new { n }
bump.call()
bump.call()
System.print(read.call())
";
    assert_eq!(run(source), "2\n");
}

#[test]
fn a_class_method_survives_a_collection() {
    // **Regression.** A class's method table was not traced by the collector,
    // so every method written in Wren was freed at the first collection --
    // the table is the only reference once the class body has finished
    // executing. It showed up as the *first* class's constructor silently
    // returning the class itself, and only once a second class pushed the
    // heap past its first collection.
    //
    // The loop is there to guarantee a collection happens rather than to hope
    // one does.
    let source = "
class Counter {
  construct new() { _n = 0 }
  bump { _n = _n + 1 }
  n { _n }
}
var c = Counter.new()
for (i in 1..500) {
  var scratch = [i, \"padding %(i)\"]
  c.bump
}
System.print(c.n)
";
    assert_eq!(run(source), "500\n");
}

#[test]
fn several_classes_each_keep_their_own_methods() {
    let source = "
class A { construct new() {} v { \"a\" } }
class B { construct new() {} v { \"b\" } }
class C { construct new() {} v { \"c\" } }
System.print(A.new().v + B.new().v + C.new().v)
";
    assert_eq!(run(source), "abc\n");
}

#[test]
fn inheritance_and_super() {
    let source = "
class Animal {
  construct new(name) { _name = name }
  speak { \"%(_name) makes a sound\" }
}
class Dog is Animal {
  construct new(name) { super(name) }
  speak { super.speak + \" (woof)\" }
}
System.print(Dog.new(\"Rex\").speak)
";
    assert_eq!(run(source), "Rex makes a sound (woof)\n");
}

#[test]
fn a_subclass_gets_its_own_fields_after_the_inherited_ones() {
    // The field-offset fix: the compiler numbers a method's fields from zero
    // because it cannot know the superclass, and the offset is applied when
    // the method is bound. Without it, `_b` would alias `_a`.
    let source = "
class Base {
  construct new() { _a = \"base\" }
  a { _a }
}
class Derived is Base {
  construct new() {
    super()
    _b = \"derived\"
  }
  b { _b }
}
var d = Derived.new()
System.print(d.a + \" \" + d.b)
";
    assert_eq!(run(source), "base derived\n");
}

#[test]
fn operator_overloading() {
    let source = "
class Vec {
  construct new(x, y) {
    _x = x
    _y = y
  }
  x { _x }
  y { _y }
  +(o) { Vec.new(_x + o.x, _y + o.y) }
  toString { \"(%(_x), %(_y))\" }
}
System.print(Vec.new(1, 2) + Vec.new(10, 20))
";
    assert_eq!(run(source), "(11, 22)\n");
}

#[test]
fn a_bare_name_in_a_method_calls_it_on_this() {
    // Which is what makes recursion inside a static method resolve.
    let source = "
class Fib {
  static get(n) {
    if (n < 2) return n
    return get(n - 1) + get(n - 2)
  }
}
System.print(Fib.get(20))
";
    assert_eq!(run(source), "6765\n");
}

#[test]
fn break_inside_a_function_inside_a_loop_is_an_error() {
    // A loop does not extend through a function boundary. This used to emit a
    // jump with the enclosing function's offsets into the inner function's
    // code, which corrupted the stack rather than reporting anything.
    let source = "
var done = false
while (!done) {
  Fn.new {
    break
  }
  done = true
}
";
    assert_eq!(error(source), "Cannot use 'break' outside of a loop.");
}

#[test]
fn continue_before_a_local_declaration_keeps_the_stack_straight() {
    // `continue` discards the locals the body added. Counting them by scope
    // depth threw away the receiver as well, and the next iteration's slots
    // were all off by one.
    let source = "
var i = 0
while (i <= 2) {
  i = i + 1
  if (i == 2) continue
  var j = i * 10
  System.print(j)
}
";
    assert_eq!(run(source), "10\n30\n");
}

// --- fibers -----------------------------------------------------------------

#[test]
fn a_fiber_runs_and_returns() {
    assert_eq!(run("var f = Fiber.new { 7 }\nSystem.print(f.call())"), "7\n");
}

#[test]
fn a_fiber_yields_and_resumes() {
    // The point of a separate stack: the call that yielded is left half
    // finished and picked up again.
    let source = "
var f = Fiber.new {
  Fiber.yield(1)
  Fiber.yield(2)
  return 3
}
System.print(f.call())
System.print(f.call())
System.print(f.call())
";
    assert_eq!(run(source), "1\n2\n3\n");
}

#[test]
fn a_fiber_reports_when_it_is_done() {
    assert_eq!(run("var f = Fiber.new { 1 }\nf.call()\nSystem.print(f.isDone)"), "true\n");
}

#[test]
fn try_catches_an_error_instead_of_propagating_it() {
    // Wren has no try/catch: an error aborts its fiber, and `try` runs one and
    // hands the error back.
    let source = "
var f = Fiber.new { 1.nope }
var error = f.try()
System.print(error)
System.print(\"still running\")
";
    assert_eq!(run(source), "Num does not implement 'nope'.\nstill running\n");
}

#[test]
fn abort_raises_a_catchable_error() {
    let source = "
var f = Fiber.new { Fiber.abort(\"deliberate\") }
System.print(f.try())
System.print(f.error)
";
    assert_eq!(run(source), "deliberate\ndeliberate\n");
}

// --- numbers print the way Wren prints them ---------------------------------

#[test]
fn numbers_use_fourteen_significant_digits() {
    // %.14g, which is what makes `0.1 + 0.2` print as `0.3` rather than as
    // `0.30000000000000004`.
    assert_eq!(run("System.print(0.1 + 0.2)"), "0.3\n");
    assert_eq!(run("System.print(2.sqrt)"), "1.4142135623731\n");
}

#[test]
fn very_large_and_small_numbers_use_exponential_notation() {
    assert_eq!(run("System.print(1e300)"), "1e+300\n");
    assert_eq!(run("System.print(1e-300)"), "1e-300\n");
}

#[test]
fn the_special_values_have_wren_spellings() {
    assert_eq!(run("System.print(1/0)"), "infinity\n");
    assert_eq!(run("System.print(-1/0)"), "-infinity\n");
    assert_eq!(run("System.print(0/0)"), "nan\n");
}

#[test]
fn negative_zero_keeps_its_sign() {
    assert_eq!(run("System.print(-0.0)"), "-0\n");
    assert_eq!(run("System.print((-0.5).truncate)"), "-0\n");
}

// --- modules ----------------------------------------------------------------

/// Run a program with a set of modules served from memory.
fn run_with_modules(source: &str, modules: &[(&'static str, &'static str)]) -> String {
    let mut vm = Vm::new();
    let table: Vec<(String, String)> = modules
        .iter()
        .map(|(name, body)| ((*name).to_string(), (*body).to_string()))
        .collect();
    vm.set_module_loader(move |wanted| {
        table
            .iter()
            .find(|(name, _)| name == wanted)
            .map(|(_, body)| body.clone())
    });
    match vm.interpret(source) {
        Ok(()) => vm.output_str().to_string(),
        Err(error) => panic!("line {}: {}", error.line(), error.message()),
    }
}

#[test]
fn a_bare_import_runs_the_module() {
    assert_eq!(
        run_with_modules("import \"m\"\nSystem.print(\"after\")", &[("m", "System.print(\"ran\")")]),
        "ran\nafter\n"
    );
}

#[test]
fn import_for_binds_named_variables() {
    assert_eq!(
        run_with_modules(
            "import \"m\" for Greeting\nSystem.print(Greeting)",
            &[("m", "var Greeting = \"hello\"")]
        ),
        "hello\n"
    );
}

#[test]
fn import_as_renames() {
    // Which is what lets two modules exporting the same name both be used.
    assert_eq!(
        run_with_modules(
            "import \"m\" for Thing as Other\nSystem.print(Other)",
            &[("m", "var Thing = 7")]
        ),
        "7\n"
    );
}

#[test]
fn a_module_runs_only_once_however_often_it_is_imported() {
    let source = "
import \"m\" for A
import \"m\" for B
System.print(A)
System.print(B)
";
    assert_eq!(
        run_with_modules(source, &[("m", "var A = 1\nvar B = 2\nSystem.print(\"ran\")")]),
        "ran\n1\n2\n"
    );
}

#[test]
fn a_module_has_its_own_namespace() {
    // The point of modules: the same name in two files is two variables, and
    // the importer sees only what it asked for.
    let source = "
var name = \"main\"
import \"m\" for exported
System.print(name)
System.print(exported)
";
    assert_eq!(
        run_with_modules(source, &[("m", "var name = \"module\"\nvar exported = name")]),
        "main\nmodule\n"
    );
}

#[test]
fn a_module_gets_the_core_library_without_importing_it() {
    assert_eq!(
        run_with_modules(
            "import \"m\" for Answer\nSystem.print(Answer)",
            &[("m", "var Answer = [1, 2].count + 40")]
        ),
        "42\n"
    );
}

#[test]
fn importing_a_name_the_module_does_not_have_is_an_error() {
    let mut vm = Vm::new();
    vm.set_module_loader(|_| Some("var Real = 1".to_string()));
    let result = vm.interpret("import \"m\" for Absent");
    assert_eq!(
        result.unwrap_err().message(),
        "Could not find a variable named 'Absent' in module 'm'."
    );
}

#[test]
fn importing_with_no_loader_fails_rather_than_panics() {
    // The default for a firmware with no filesystem.
    let mut vm = Vm::new();
    assert_eq!(
        vm.interpret("import \"m\"").unwrap_err().message(),
        "Could not load module 'm'."
    );
}

// --- static fields ----------------------------------------------------------

#[test]
fn a_static_field_is_shared_by_every_instance() {
    let source = "
class Counter {
  construct new() {}
  bump { __n = (__n == null) ? 1 : __n + 1 }
  n { __n }
}
Counter.new().bump
Counter.new().bump
System.print(Counter.new().n)
";
    assert_eq!(run(source), "2\n");
}

#[test]
fn a_static_field_is_visible_from_static_and_instance_methods_alike() {
    let source = "
class Both {
  construct new() {}
  static set { __v = \"set statically\" }
  read { __v }
}
Both.set
System.print(Both.new().read)
";
    assert_eq!(run(source), "set statically\n");
}

#[test]
fn an_unset_static_field_reads_as_null() {
    assert_eq!(
        run("class A {\n construct new() {}\n v { __missing }\n}\nSystem.print(A.new().v)"),
        "null\n"
    );
}

#[test]
fn a_nested_class_has_its_own_static_fields() {
    // The storage is on the class a method was *defined* in, so an inner class
    // writing `__field` cannot disturb the outer one's.
    let source = "
class Outer {
  static go {
    __field = \"outer\"
    class Inner {
      static go { __field = \"inner\" }
    }
    Inner.go
    System.print(__field)
  }
}
Outer.go
";
    assert_eq!(run(source), "outer\n");
}

#[test]
fn a_static_field_outside_a_class_is_an_error() {
    assert_eq!(
        error("__nope = 1"),
        "Cannot use a static field outside of a class definition."
    );
}

// --- the collector, against the things it has already missed ----------------

#[test]
fn a_suspended_fiber_survives_a_collection() {
    // **Regression.** While a fiber runs, its stack and frames live on the VM
    // rather than in the heap object, so the object is referenced from nowhere
    // else -- the root fiber especially, which no program names. Collecting it
    // left `Fiber.yield` with no caller to return to.
    let source = "
var f = Fiber.new {
  Fiber.yield(1)
  Fiber.yield(2)
  return 3
}
System.print(f.call())
for (i in 1..400) {
  var scratch = [i, \"padding %(i)\"]
}
System.print(f.call())
System.print(f.call())
";
    assert_eq!(run(source), "1\n2\n3\n");
}

#[test]
fn an_imported_variable_survives_a_collection() {
    let source = "
import \"m\" for Held
for (i in 1..400) {
  var scratch = [i, \"padding %(i)\"]
}
System.print(Held)
";
    assert_eq!(
        run_with_modules(source, &[("m", "var Held = \"still here\"")]),
        "still here\n"
    );
}

// --- the random module ------------------------------------------------------

#[test]
fn random_floats_are_in_range() {
    let source = "
import \"random\" for Random
var r = Random.new(12345)
var ok = 0
for (i in 1..200) {
  var n = r.float()
  if (n >= 0 && n < 1) ok = ok + 1
}
System.print(ok)
";
    assert_eq!(run(source), "200\n");
}

#[test]
fn random_ints_respect_their_bounds() {
    let source = "
import \"random\" for Random
var r = Random.new(99)
var ok = 0
for (i in 1..200) {
  var n = r.int(10, 20)
  if (n >= 10 && n < 20) ok = ok + 1
}
System.print(ok)
";
    assert_eq!(run(source), "200\n");
}

#[test]
fn a_seeded_generator_repeats_itself() {
    // Not required by upstream's tests, which only ask for distribution -- but
    // a generator that cannot be reproduced from a seed is not much use for
    // debugging whatever it feeds.
    let source = "
import \"random\" for Random
var a = Random.new(7)
var b = Random.new(7)
var same = 0
for (i in 1..50) {
  if (a.float() == b.float()) same = same + 1
}
System.print(same)
";
    assert_eq!(run(source), "50\n");
}

#[test]
fn shuffle_keeps_every_element() {
    let source = "
import \"random\" for Random
var r = Random.new(3)
var list = [1, 2, 3, 4, 5]
r.shuffle(list)
var total = 0
for (x in list) total = total + x
System.print(list.count)
System.print(total)
";
    assert_eq!(run(source), "5\n15\n");
}

// --- sequences --------------------------------------------------------------

#[test]
fn a_class_answering_the_protocol_is_a_sequence() {
    // **The whole contract is two methods.** A class that answers `iterate(_)`
    // and `iteratorValue(_)` gets everything else from `Sequence`.
    let source = "
class Countdown is Sequence {
  construct new(from) { _from = from }
  iterate(i) {
    if (i == null) return _from
    if (i <= 1) return false
    return i - 1
  }
  iteratorValue(i) { i }
}
System.print(Countdown.new(3).toList)
System.print(Countdown.new(3).count)
";
    assert_eq!(run(source), "[3, 2, 1]\n3\n");
}

#[test]
fn map_is_lazy() {
    // The test that decides this cannot return a list: the source never ends,
    // so anything eager would run until it ran out of memory.
    let source = "
class Naturals is Sequence {
  construct new() {}
  iterate(i) { i == null ? 1 : i + 1 }
  iteratorValue(i) { i }
}
System.print(Naturals.new().map { |n| n * 2 }.take(4).toList)
";
    assert_eq!(run(source), "[2, 4, 6, 8]\n");
}

#[test]
fn where_is_lazy_too() {
    let source = "
class Naturals is Sequence {
  construct new() {}
  iterate(i) { i == null ? 1 : i + 1 }
  iteratorValue(i) { i }
}
System.print(Naturals.new().where { |n| n % 3 == 0 }.take(3).toList)
";
    assert_eq!(run(source), "[3, 6, 9]\n");
}

#[test]
fn skip_and_take_compose() {
    assert_eq!(run("System.print((1..10).skip(2).take(3).toList)"), "[3, 4, 5]\n");
}

#[test]
fn sequence_methods_work_on_every_built_in_collection() {
    // List, Range, Map and String all inherit from Sequence, so the same
    // methods reach all of them.
    assert_eq!(run("System.print([1, 2, 3].map { |x| x * 2 }.toList)"), "[2, 4, 6]\n");
    assert_eq!(run("System.print((1..4).where { |x| x % 2 == 0 }.toList)"), "[2, 4]\n");
    assert_eq!(run("System.print(\"abc\".toList)"), "[a, b, c]\n");
    assert_eq!(run("System.print((1..3).reduce { |a, b| a + b })"), "6\n");
    assert_eq!(run("System.print([1, 2, 3].join(\"-\"))"), "1-2-3\n");
}

#[test]
fn a_map_is_a_sequence_of_entries() {
    assert_eq!(run("var m = {\"a\": 1}\nSystem.print(m.count)"), "1\n");
}

#[test]
fn sequence_predicates() {
    assert_eq!(run("System.print((1..3).all { |x| x > 0 })"), "true\n");
    assert_eq!(run("System.print((1..3).any { |x| x > 2 })"), "true\n");
    assert_eq!(run("System.print((1..3).contains(2))"), "true\n");
    assert_eq!(run("System.print([].isEmpty)"), "true\n");
}

#[test]
fn take_and_skip_reject_a_bad_count() {
    assert_eq!(error("(1..3).take(-1)"), "Count must be a non-negative integer.");
    assert_eq!(error("(1..3).take(1.5)"), "Count must be an integer.");
    assert_eq!(error("(1..3).skip(\"two\")"), "Count must be a number.");
}

#[test]
fn a_map_key_must_be_a_value_type() {
    // A list is excluded because a key is compared by contents, and a mutable
    // key's contents can change after insertion.
    assert_eq!(error("var m = {}\nm[[1]] = 2"), "Key must be a value type.");
}

#[test]
fn ranges_work_as_map_keys() {
    // Which needs range equality by value: two separately built `1..3` are the
    // same key.
    assert_eq!(run("var m = {}\nm[1..3] = \"yes\"\nSystem.print(m[1..3])"), "yes\n");
}

#[test]
fn a_map_prints_as_a_literal() {
    assert_eq!(run("System.print({})"), "{}\n");
    assert_eq!(run("System.print({\"a\": 1})"), "{a: 1}\n");
}

#[test]
fn a_list_prints_its_elements_with_their_own_tostring() {
    let source = "
class Named {
  construct new() {}
  toString { \"named!\" }
}
System.print([1, Named.new()])
";
    assert_eq!(run(source), "[1, named!]\n");
}

// --- the map's hash table ---------------------------------------------------

#[test]
fn a_map_finds_keys_after_many_insertions() {
    // Enough entries to force several growths, each rehashing everything.
    let source = "
var m = {}
for (i in 1..200) m[i] = i * 2
var total = 0
for (i in 1..200) total = total + m[i]
System.print(m.count)
System.print(total)
";
    assert_eq!(run(source), "200\n40200\n");
}

#[test]
fn removing_an_entry_does_not_strand_the_ones_behind_it() {
    // **The tombstone test.** A key that collided with the removed one probed
    // past its slot on the way in. Blanking the slot rather than marking it
    // would end that probe early and lose the later key entirely -- and only
    // for keys that happened to collide, which is why it needs a lot of them
    // rather than a hand-picked pair.
    let source = "
var m = {}
for (i in 1..100) m[i] = i
for (i in 1..100) {
  if (i % 3 == 0) m.remove(i)
}
var found = 0
for (i in 1..100) {
  if (i % 3 != 0 && m[i] == i) found = found + 1
}
System.print(found)
System.print(m.count)
";
    assert_eq!(run(source), "67\n67\n");
}

#[test]
fn a_slot_can_be_reused_after_removal() {
    let source = "
var m = {}
for (i in 1..50) {
  m[\"key\"] = i
  m.remove(\"key\")
}
m[\"key\"] = \"last\"
System.print(m.count)
System.print(m[\"key\"])
";
    assert_eq!(run(source), "1\nlast\n");
}

#[test]
fn keys_of_every_value_type_work() {
    let source = "
var m = {}
m[1] = \"num\"
m[\"s\"] = \"string\"
m[true] = \"bool\"
m[null] = \"null\"
m[1..2] = \"range\"
System.print(m[1])
System.print(m[\"s\"])
System.print(m[true])
System.print(m[null])
System.print(m[1..2])
System.print(m.count)
";
    assert_eq!(run(source), "num\nstring\nbool\nnull\nrange\n5\n");
}

#[test]
fn negative_zero_and_zero_are_the_same_key() {
    // They are `==`, so they must hash alike or one would be unreachable.
    assert_eq!(run("var m = {}\nm[0] = \"a\"\nm[-0.0] = \"b\"\nSystem.print(m.count)\nSystem.print(m[0])"), "1\nb\n");
}

#[test]
fn iterating_a_map_visits_every_entry_once() {
    let source = "
var m = {}
for (i in 1..30) m[i] = i
var seen = 0
var total = 0
for (entry in m) {
  seen = seen + 1
  total = total + entry.value
}
System.print(seen)
System.print(total)
";
    assert_eq!(run(source), "30\n465\n");
}

#[test]
fn an_invalid_map_iterator_is_an_error() {
    assert_eq!(
        error("var m = {}\nm[1] = 1\nm.iteratorValue(500)"),
        "Invalid map iterator."
    );
}
