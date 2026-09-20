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
    assert_eq!(
        run("System.print(10 - 4 - 3)"),
        "3\n",
        "minus is left-associative"
    );
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
    assert_eq!(
        run("System.print(!\"\")"),
        "false\n",
        "the empty string is truthy"
    );
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
    assert_eq!(
        run("if (3 > 2) System.print(\"big\") else System.print(\"small\")"),
        "big\n"
    );
    assert_eq!(
        run("if (1 > 2) System.print(\"big\") else System.print(\"small\")"),
        "small\n"
    );
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
    assert_eq!(
        run("var t = 0\nfor (i in 1..10) t = t + i\nSystem.print(t)"),
        "55\n"
    );
}

// --- lists ------------------------------------------------------------------

#[test]
fn list_literal_and_indexing() {
    assert_eq!(run("var l = [1, 2, 3]\nSystem.print(l[0])"), "1\n");
    assert_eq!(
        run("var l = [1, 2, 3]\nSystem.print(l[-1])"),
        "3\n",
        "negative indexes count back"
    );
}

#[test]
fn list_index_assignment() {
    assert_eq!(run("var l = [1, 2]\nl[0] = 9\nSystem.print(l[0])"), "9\n");
}

#[test]
fn list_add_and_count() {
    assert_eq!(
        run("var l = []\nl.add(1)\nl.add(2)\nSystem.print(l.count)"),
        "2\n"
    );
}

#[test]
fn a_list_prints_its_elements() {
    assert_eq!(run("System.print([1, \"two\", null])"), "[1, two, null]\n");
}

#[test]
fn an_out_of_range_index_is_a_runtime_error() {
    assert_eq!(
        error("var l = [1]\nSystem.print(l[5])"),
        "Index out of bounds."
    );
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
    assert_eq!(
        error("System.print(\"a\" + 1)"),
        "Right operand must be a string."
    );
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
    assert_eq!(
        error("System.print(1.nope)"),
        "Num does not implement 'nope'."
    );
}

#[test]
fn a_runtime_error_reports_its_line() {
    let mut vm = Vm::new();
    let result = vm.interpret("var a = 1\nvar b = 2\nSystem.print(a.nope)");
    assert_eq!(result.unwrap_err().line(), 3);
}

/// **The line table is one entry per line, not per byte**, so reporting a line
/// is a search rather than an index -- see `Chunk::line_at`. These pin the
/// cases that search has to get right: a line well into a program, a line
/// reached only after a backward jump, and a line inside a called function,
/// none of which the conformance suite checks because its error tests pass on
/// any error at all.
#[test]
fn a_runtime_error_reports_a_line_far_into_the_program() {
    let mut vm = Vm::new();
    let source = (1..20)
        .map(|n| format!("var v{n} = {n}"))
        .collect::<Vec<_>>()
        .join("\n");
    let result = vm.interpret(&format!("{source}\nSystem.print(v1.nope)"));
    assert_eq!(result.unwrap_err().line(), 20);
}

#[test]
fn a_runtime_error_inside_a_loop_reports_its_line() {
    let mut vm = Vm::new();
    let result = vm.interpret("var total = 0\nfor (i in 1..3) {\n  total = total + i\n}\ntotal.nope");
    assert_eq!(result.unwrap_err().line(), 5);
}

#[test]
fn a_runtime_error_inside_a_function_reports_its_line() {
    let mut vm = Vm::new();
    let result = vm.interpret("var f = Fn.new {\n  var x = 1\n  x.nope\n}\nf.call()");
    assert_eq!(result.unwrap_err().line(), 3);
}

#[test]
fn wrong_operand_type() {
    assert_eq!(
        error("System.print(1 + \"a\")"),
        "Right operand must be a number."
    );
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
    assert_eq!(
        run("var f = Fn.new { |a, b| a + b }\nSystem.print(f.call(1, 2))"),
        "3\n"
    );
}

#[test]
fn a_closure_sees_later_writes_to_what_it_captured() {
    // Capturing takes the variable, not a copy of its value at the time.
    assert_eq!(
        run("var n = 1\nvar f = Fn.new { n }\nn = 2\nSystem.print(f.call())"),
        "2\n"
    );
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
    assert_eq!(
        run("var f = Fiber.new { 7 }\nSystem.print(f.call())"),
        "7\n"
    );
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
    assert_eq!(
        run("var f = Fiber.new { 1 }\nf.call()\nSystem.print(f.isDone)"),
        "true\n"
    );
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
    assert_eq!(
        run(source),
        "Num does not implement 'nope'.\nstill running\n"
    );
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

// **Fourteen digits is the double's rule.** A 32-bit build prints to eight
// and these two would be asserting the wrong answer there, so they are skipped
// rather than made to agree with whatever the build does.
#[cfg(not(feature = "f32"))]
#[test]
fn numbers_use_fourteen_significant_digits() {
    // %.14g, which is what makes `0.1 + 0.2` print as `0.3` rather than as
    // `0.30000000000000004`.
    assert_eq!(run("System.print(0.1 + 0.2)"), "0.3\n");
    assert_eq!(run("System.print(2.sqrt)"), "1.4142135623731\n");
}

#[cfg(not(feature = "f32"))]
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
        run_with_modules(
            "import \"m\"\nSystem.print(\"after\")",
            &[("m", "System.print(\"ran\")")]
        ),
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
        run_with_modules(
            source,
            &[("m", "var A = 1\nvar B = 2\nSystem.print(\"ran\")")]
        ),
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
        run_with_modules(
            source,
            &[("m", "var name = \"module\"\nvar exported = name")]
        ),
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
    assert_eq!(
        run("System.print((1..10).skip(2).take(3).toList)"),
        "[3, 4, 5]\n"
    );
}

#[test]
fn sequence_methods_work_on_every_built_in_collection() {
    // List, Range, Map and String all inherit from Sequence, so the same
    // methods reach all of them.
    assert_eq!(
        run("System.print([1, 2, 3].map { |x| x * 2 }.toList)"),
        "[2, 4, 6]\n"
    );
    assert_eq!(
        run("System.print((1..4).where { |x| x % 2 == 0 }.toList)"),
        "[2, 4]\n"
    );
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
    assert_eq!(
        error("(1..3).take(-1)"),
        "Count must be a non-negative integer."
    );
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
    assert_eq!(
        run("var m = {}\nm[1..3] = \"yes\"\nSystem.print(m[1..3])"),
        "yes\n"
    );
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
    assert_eq!(
        run("var m = {}\nm[0] = \"a\"\nm[-0.0] = \"b\"\nSystem.print(m.count)\nSystem.print(m[0])"),
        "1\nb\n"
    );
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
    // **Two different faults, two different messages.** Past the end of the
    // table is a bad index; inside it but pointing at an empty slot is an
    // iterator that has gone stale. A one-entry map has a table of eight, so
    // slot 7 is in range and empty.
    assert_eq!(
        error("var m = {}\nm[1] = 1\nm.iteratorValue(500)"),
        "Iterator out of bounds."
    );
    assert_eq!(
        error("var m = {}\nm[1] = 1\nm.iteratorValue(7)"),
        "Invalid map iterator."
    );
}

// --- what the language refuses ----------------------------------------------

#[test]
fn a_class_cannot_inherit_from_a_built_in() {
    // Their instances have a representation of their own -- a Num is a double
    // in the value itself -- so an instance of a subclass would have to be
    // both that and an object with fields.
    assert_eq!(
        error("class Sub is Num {}"),
        "Class 'Sub' cannot inherit from built-in class 'Num'."
    );
    assert_eq!(
        error("class Sub is String {}"),
        "Class 'Sub' cannot inherit from built-in class 'String'."
    );
}

#[test]
fn a_class_may_inherit_from_object_and_sequence() {
    // The two that are not representations, only behaviour.
    assert_eq!(
        run("class A is Object { construct new() {} }\nSystem.print(A.new() is A)"),
        "true\n"
    );
    assert_eq!(
        run("class B is Sequence { construct new() {} }\nSystem.print(B.new() is Sequence)"),
        "true\n"
    );
}

#[test]
fn a_constructor_must_be_a_named_method() {
    // Every other shape is called on an instance that already exists, which is
    // the one thing a constructor does not have.
    assert_eq!(
        error("class A { construct +(o) {} }"),
        "A constructor cannot be an operator."
    );
    assert_eq!(
        error("class A { construct v { } }"),
        "A constructor cannot be a getter."
    );
    assert_eq!(
        error("class A { construct v=(x) {} }"),
        "A constructor cannot be a setter."
    );
    assert_eq!(
        error("class A { construct [i] {} }"),
        "A constructor cannot be a subscript."
    );
    assert_eq!(
        error("class A { static construct new() {} }"),
        "A constructor cannot be static."
    );
}

#[test]
fn a_bad_escape_is_a_compile_error() {
    // It used to keep both characters, which quietly turned a typo into
    // output.
    assert_eq!(
        error("System.print(\"\\q\")"),
        "Invalid escape character 'q'."
    );
    assert_eq!(
        error("System.print(\"\\x1\")"),
        "Incomplete byte escape sequence."
    );
    assert_eq!(
        error("System.print(\"\\xzz\")"),
        "Invalid byte escape sequence."
    );
    assert_eq!(
        error("System.print(\"\\u12\")"),
        "Incomplete Unicode escape sequence."
    );
    assert_eq!(
        error("System.print(\"\\uzzzz\")"),
        "Invalid Unicode escape sequence."
    );
}

#[test]
fn good_escapes_still_work() {
    assert_eq!(run("System.print(\"\\x41\")"), "A\n");
    assert_eq!(run("System.print(\"\\u0041\")"), "A\n");
    assert_eq!(run("System.print(\"\\U00000041\")"), "A\n");
}

// --- syntax that spans lines ------------------------------------------------

#[test]
fn a_call_chain_may_be_broken_across_lines() {
    let source = "
class Chain {
  construct new() {}
  a { this }
  b { this }
  done { \"chained\" }
}
System.print(Chain.new()
  .a
  .b
  .done)
";
    assert_eq!(run(source), "chained\n");
}

#[test]
fn a_byte_order_mark_is_not_a_token() {
    // Editors add one; leaving it in made the first token of an otherwise
    // valid program an error nobody could see.
    assert_eq!(run("\u{feff}System.print(1)"), "1\n");
}

#[test]
fn a_method_wins_over_a_module_variable_of_the_same_name() {
    // Upstream's resolution order: locals, then the implicit receiver, then
    // the module. Checking the module first resolved `foo` to the class.
    let source = "
class foo {
  construct new() {}
  static bar { \"static method\" }
  baz { bar }
}
System.print(foo.bar)
";
    assert_eq!(run(source), "static method\n");
}

// --- more of what the language refuses --------------------------------------

#[test]
fn string_methods_reject_a_non_string_argument() {
    assert_eq!(error("\"abc\".contains(1)"), "Argument must be a string.");
    assert_eq!(error("\"abc\".startsWith(1)"), "Argument must be a string.");
    assert_eq!(error("\"abc\".indexOf(1)"), "Argument must be a string.");
}

#[test]
fn from_code_point_checks_its_range() {
    assert_eq!(
        error("String.fromCodePoint(-1)"),
        "Code point cannot be negative."
    );
    assert_eq!(
        error("String.fromCodePoint(1114112)"),
        "Code point cannot be greater than 0x10ffff."
    );
    assert_eq!(
        error("String.fromCodePoint(1.5)"),
        "Code point must be an integer."
    );
}

#[test]
fn a_constructor_cannot_return_a_value() {
    // Its body always yields the instance, so the value would be discarded --
    // which makes writing one a mistake rather than a choice.
    assert_eq!(
        error("class A {\n construct new() {\n  return 1\n }\n}"),
        "A constructor cannot return a value."
    );
    // A bare `return` is fine: it just ends the body early.
    assert_eq!(
        run("class A {\n construct new() {\n  return\n }\n}\nSystem.print(A.new() is A)"),
        "true\n"
    );
}

#[test]
fn a_class_cannot_define_the_same_method_twice() {
    // Silently replacing the first would look like it simply never ran.
    assert_eq!(
        error("class A {\n v { 1 }\n v { 2 }\n}"),
        "Class A already defines a method 'v'."
    );
    // A static and an instance method may share a name; they are different
    // tables.
    assert_eq!(
        run("class A {\n construct new() {}\n static v { 1 }\n v { 2 }\n}\nSystem.print(A.v)\nSystem.print(A.new().v)"),
        "1\n2\n"
    );
}

#[test]
fn an_unterminated_block_comment_is_an_error() {
    // It used to swallow the rest of the file quietly, so a missing two
    // characters compiled to an empty program.
    assert_eq!(error("/* never closed\nSystem.print(1)"), "Invalid token.");
}

#[test]
fn block_comments_nest() {
    assert_eq!(run("/* a /* b */ c */\nSystem.print(1)"), "1\n");
}

#[test]
fn a_subscript_must_take_a_parameter() {
    assert_eq!(
        error("class A { [] { 1 } }"),
        "Expect subscript parameters."
    );
}

// --- fibers and closures over the same variable -----------------------------

#[test]
fn a_fiber_and_a_closure_share_a_captured_variable() {
    // **The hardest capture case.** A fiber and a closure both close over `a`,
    // the fiber writes to it between yields, and the closure must see each
    // write. It also caught a real bug: returning from a nested Rust call --
    // `closure.call()` re-enters the interpreter -- was marking the fiber that
    // merely *contained* the call as finished.
    let source = "
var fiber
var closure
{
  var a = \"before\"
  fiber = Fiber.new {
    Fiber.yield()
    a = \"after\"
    Fiber.yield()
    a = \"final\"
  }
  closure = Fn.new { a }
}
fiber.call()
System.print(closure.call())
fiber.call()
System.print(closure.call())
fiber.call()
System.print(closure.call())
";
    assert_eq!(run(source), "before\nafter\nfinal\n");
}

#[test]
fn calling_a_function_does_not_finish_the_fiber_around_it() {
    let source = "
var f = Fiber.new {
  Fiber.yield(1)
  return 2
}
System.print(f.call())
System.print(Fn.new { \"between\" }.call())
System.print(f.isDone)
System.print(f.call())
";
    assert_eq!(run(source), "1\nbetween\nfalse\n2\n");
}

// --- the conditional operator's precedence ----------------------------------

#[test]
fn a_conditional_may_not_nest_in_its_own_then_branch() {
    // `?` binds at assignment precedence and the then-branch is parsed one
    // level tighter, so this is an error rather than quietly grouping one of
    // the two possible ways.
    assert_eq!(
        error("1 ? 2 ? 3 : 4 : 5"),
        "Expect ':' after then branch of conditional operator."
    );
    // Parenthesised, it is fine.
    assert_eq!(run("System.print(1 ? (2 ? 3 : 4) : 5)"), "3\n");
}

#[test]
fn a_conditional_binds_looser_than_everything_but_assignment() {
    assert_eq!(run("System.print(3 + 4 ? 1 : 2)"), "1\n");
    assert_eq!(run("System.print(3 is Num ? 1 : 2)"), "1\n");
    assert_eq!(run("var a = 0\nSystem.print(a = 3 ? 1 : 2)"), "1\n");
}

#[test]
fn a_call_chain_may_break_after_the_dot_as_well_as_before_it() {
    let source = "
class Chain {
  construct new() {}
  a { this }
  done { \"chained\" }
}
System.print(Chain.new().
  a.
  done)
";
    assert_eq!(run(source), "chained\n");
}

#[test]
fn a_superclass_constructor_needs_an_argument_list() {
    assert_eq!(
        error(
            "class A {\n construct new() {}\n}\nclass B is A {\n construct new() {\n  super\n }\n}"
        ),
        "A superclass constructor must have an argument list."
    );
}

#[test]
fn gc_can_be_forced() {
    // Provoking a collection beats allocating until one happens by luck.
    let source = "
class Holder {
  construct new(v) { _v = v }
  v { _v }
}
var kept = Holder.new(\"kept\")
System.gc()
System.print(kept.v)
";
    assert_eq!(run(source), "kept\n");
}

// --- strings are bytes, and are walked as UTF-8 where they can be -----------

#[test]
fn a_string_is_eight_bit_clean() {
    // `\xff` is one byte, not the two bytes of U+00FF. Decoding escapes into a
    // Rust String turned every high byte into its Latin-1 character, which is
    // a different string from the one written.
    assert_eq!(run("System.print(\"\\0\".bytes.count)"), "1\n");
    assert_eq!(run("System.print(\"\\xff\".bytes.count)"), "1\n");
    assert_eq!(run("System.print(\"\\xff\".bytes[0])"), "255\n");
    assert_eq!(run("System.print(\"a\\0b\".bytes.count)"), "3\n");
}

#[test]
fn count_is_code_points_and_bytes_is_bytes() {
    // Two different questions. `count` treats a UTF-8 sequence as one item.
    assert_eq!(run("System.print(\"søméஃthîng\".count)"), "10\n");
    assert_eq!(run("System.print(\"søméஃthîng\".bytes.count)"), "15\n");
}

#[test]
fn invalid_utf8_counts_one_byte_at_a_time() {
    // `\xef` is a three-byte lead, but what follows are not continuation
    // bytes, so there is no sequence there. Trusting the lead byte's implied
    // length would step over the `o` and the `k`.
    assert_eq!(run("System.print(\"\\xefok\\xf7\".count)"), "4\n");
}

#[test]
fn iterating_a_string_yields_whole_characters() {
    let source = "
var out = []
for (c in \"søm\") out.add(c)
System.print(out.count)
System.print(out[1])
";
    assert_eq!(run(source), "3\nø\n");
}

#[test]
fn slicing_a_string_drops_partial_sequences() {
    // **Byte positions in, whole characters out.** Upstream visits each
    // selected byte and emits a code point only where one starts, so a range
    // beginning or ending mid-sequence drops those bytes rather than
    // producing half a character.
    assert_eq!(run("System.print(\"søméஃthîng\"[0..3])"), "søm\n");
    assert_eq!(run("System.print(\"søméஃthîng\"[2..6])"), "méஃ\n");
    assert_eq!(run("System.print(\"søméஃthîng\"[2...6])"), "mé\n");
}

#[test]
fn string_search_and_replace() {
    assert_eq!(run("System.print(\"abcd\".indexOf(\"cd\", 0))"), "2\n");
    assert_eq!(run("System.print(\"abcd\".indexOf(\"cd\", 3))"), "-1\n");
    assert_eq!(run("System.print(\"aaaaa\".indexOf(\"aaaa\", 1))"), "1\n");
    assert_eq!(
        run("System.print(\"a-b-c\".replace(\"-\", \"+\"))"),
        "a+b+c\n"
    );
}

/// **A shipped build need not carry a map back to its source.**
/// `wrenc --strip-lines` leaves the line table out: the program runs the same
/// and a runtime error reports line 0. See `wrenc::Lines`.
#[test]
fn stripped_bytecode_runs_the_same_and_reports_no_line() {
    let source = "var a = 1\nvar b = 2\nSystem.print(a + b)\na.nope\n";

    let run = |lines: wren::wrenc::Lines| {
        let mut vm = Vm::new();
        let chunk = wren::compiler::compile(&mut vm, source).expect("it compiles");
        let bytes = wren::wrenc::write_with(&vm, &chunk, source.as_bytes(), lines)
            .expect("it serialises");

        let mut vm = Vm::new();
        let loaded = wren::wrenc::load(&mut vm, &bytes).expect("it loads");
        let error = vm
            .run_closure(loaded.closure)
            .expect_err("the last line fails");
        (vm.output_str().to_string(), error.line)
    };

    let (kept_output, kept_line) = run(wren::wrenc::Lines::Keep);
    let (stripped_output, stripped_line) = run(wren::wrenc::Lines::Strip);

    assert_eq!(kept_output, stripped_output, "the program behaves the same");
    assert_eq!(kept_line, 4, "with the table, the error knows its line");
    assert_eq!(stripped_line, 0, "without it, there is no line to report");
}

/// **Capturing an upvalue is its own instruction now**, emitted after the
/// `Closure` it belongs to rather than carried as payload inside it -- see
/// `Op::CaptureLocal`. These pin both halves: that capture still works, and
/// that it survives a round trip through the `.wrenc` format, whose walker
/// has to agree with the compiler about how long an instruction is.
#[test]
fn a_closure_captures_through_a_bytecode_round_trip() {
    let source = "var make = Fn.new { |n|\n  return Fn.new { n * 2 }\n}\nSystem.print(make.call(21).call())\n";

    let mut direct = Vm::new();
    direct.interpret(source).expect("it runs");
    assert_eq!(direct.output_str().trim(), "42", "run straight from source");

    let mut vm = Vm::new();
    let chunk = wren::compiler::compile(&mut vm, source).expect("it compiles");
    let bytes = wren::wrenc::write(&vm, &chunk, source.as_bytes()).expect("it serialises");

    let mut loaded_vm = Vm::new();
    let loaded = wren::wrenc::load(&mut loaded_vm, &bytes).expect("it loads");
    loaded_vm.run_closure(loaded.closure).expect("it runs");
    assert_eq!(loaded_vm.output_str().trim(), "42", "run from bytecode");
}

#[test]
fn several_upvalues_are_captured_in_order() {
    let mut vm = Vm::new();
    vm.interpret(
        "var make = Fn.new { |a, b, c|\n  return Fn.new { a + b * c }\n}\nSystem.print(make.call(1, 2, 3).call())\n",
    )
    .expect("it runs");
    assert_eq!(vm.output_str().trim(), "7");
}

// --- heap settings that have to precede the first allocation ---------------

/// A block size set on the heap survives being handed to a VM.
///
/// **Building a VM fills its heap** -- the core classes and their names are
/// the first two dozen objects in any program -- so `set_slot_block` is
/// already too late by the time `Vm::new` returns. `Vm::with_heap` is the way
/// to ask for one, and this is the test that it actually arrives, rather than
/// being quietly reset to the default.
#[cfg(feature = "blocked-slots")]
#[test]
fn a_heap_carries_its_block_size_into_the_vm() {
    let mut heap = wren::heap::Heap::new();
    assert!(heap.set_slot_block(8), "an empty heap takes the setting");

    let mut vm = Vm::with_heap(heap);
    assert_eq!(vm.heap.slot_block(), Some(8), "and keeps it");

    // Enough objects to fill many blocks, held and then let go, so that blocks
    // are made, given back and made again under a non-default split.
    let source = r#"
        var total = 0
        for (round in 1..3) {
            var items = []
            for (i in 1..400) { items.add("item %(i)") }
            for (item in items) { total = total + item.count }
        }
        System.print(total)
    "#;
    match vm.interpret(source) {
        // 400 strings of "item " plus the digits of 1..400, three times over:
        // 3 x (400 x 5 + 9 + 90 x 2 + 301 x 3).
        Ok(()) => assert_eq!(vm.output_str(), "9276\n"),
        Err(error) => panic!("line {}: {}", error.line(), error.message()),
    }

    assert!(
        !vm.heap.set_slot_block(16),
        "and refuses to move once the VM's own objects are in it"
    );
}

/// A compiled chunk can be run more than once.
///
/// **This is what lets a benchmark repeat without recompiling.** A short
/// program measured on a device is mostly its own compile time otherwise, and
/// running it ten times from ten compiles dilutes nothing. Re-running one
/// chunk re-executes the module initialisers -- classes are made again and
/// module variables overwritten in place -- which is exactly the behaviour a
/// repeat wants, and is not obviously true until it is checked.
#[test]
fn a_compiled_chunk_can_be_run_again() {
    use std::rc::Rc;
    let mut vm = Vm::new();
    let chunk = Rc::new(
        wren::compiler::compile(
            &mut vm,
            r#"
                class Counter {
                    construct new() { _n = 0 }
                    bump { _n = _n + 1 }
                    n { _n }
                }
                var counter = Counter.new()
                counter.bump
                counter.bump
                System.print(counter.n)
            "#,
        )
        .expect("compiles"),
    );

    for round in 1..=3 {
        vm.run(chunk.clone())
            .unwrap_or_else(|error| panic!("round {round}: {}", error.message));
    }
    assert_eq!(
        vm.output_str(),
        "2\n2\n2\n",
        "each run starts from the module initialisers, so each prints 2"
    );
}

// --- the arithmetic fast path ----------------------------------------------

/// The fast path may skip work; it must never change an answer.
///
/// **Each of these is a case where the operands are not two numbers**, so the
/// `Call` arm has to fall through to real dispatch. They are written out
/// because the fast path is taken *before* the receiver's class is known --
/// that is the whole of its speed -- so the guard that sends these down the
/// slow path is the only thing keeping them correct.
#[test]
fn operators_that_are_not_numeric_still_dispatch() {
    assert_eq!(run(r#"System.print("a" + "b")"#), "ab\n", "string concatenation");
    assert_eq!(run("System.print([1] + [2])"), "[1, 2]\n", "list concatenation");
    let money = r#"
        class Money {
            construct new(amount) { _amount = amount }
            amount { _amount }
            +(other) {
                return Money.new(_amount + other.amount)
            }
        }
        System.print((Money.new(2) + Money.new(3)).amount)
    "#;
    assert_eq!(run(money), "5\n", "a user class defining an operator");
    assert_eq!(
        error("System.print(1 + \"x\")"),
        "Right operand must be a number.",
        "a number and a non-number still raises the primitive's own error"
    );
    assert_eq!(
        error("System.print(\"x\" - 1)"),
        "String does not implement '-(_)'.",
        "a non-number receiver is still a missing method"
    );
}

/// The inlined arithmetic agrees with the primitive it replaces.
///
/// Including the corners: `%` follows C's fmod and takes the sign of the
/// dividend, division by zero is an infinity rather than a fault, and every
/// comparison against NaN is false.
#[test]
fn inlined_arithmetic_matches_the_primitive() {
    assert_eq!(run("System.print(7 % 3)"), "1\n");
    assert_eq!(run("System.print(-7 % 3)"), "-1\n", "sign of the dividend");
    assert_eq!(run("System.print(1 / 0)"), "infinity\n");
    assert_eq!(run("System.print(2 - 5)"), "-3\n");
    assert_eq!(run("System.print(2.5 * 4)"), "10\n");
    assert_eq!(run("System.print(1 < 2)"), "true\n");
    assert_eq!(run("System.print(2 <= 2)"), "true\n");
    assert_eq!(run("System.print(3 > 4)"), "false\n");
    assert_eq!(run("System.print(3 >= 4)"), "false\n");
    let nan = "var n = 0 / 0\n";
    assert_eq!(run(&format!("{nan}System.print(n < 1)")), "false\n");
    assert_eq!(run(&format!("{nan}System.print(n >= 1)")), "false\n");
    assert_eq!(run(&format!("{nan}System.print(n <= n)")), "false\n");
}

/// `!` is three primitives, and the fast path answers for two of them.
///
/// `Bool` and `Null` both define `!` and neither can be reopened, so those
/// two are settled. `Object.!` answers `false` for everything else -- which
/// is how `!0` and `!""` are `false` in Wren -- and a user class may override
/// it, so both of those must still dispatch.
#[test]
fn negation_answers_for_bool_and_null_and_dispatches_for_the_rest() {
    assert_eq!(run("System.print(!true)"), "false\n");
    assert_eq!(run("System.print(!false)"), "true\n");
    assert_eq!(run("System.print(!null)"), "true\n", "null is falsy");
    assert_eq!(run("System.print(!!true)"), "true\n", "and it composes");

    assert_eq!(run("System.print(!0)"), "false\n", "zero is truthy in Wren");
    assert_eq!(run(r#"System.print(!"")"#), "false\n", "so is the empty string");
    assert_eq!(run("System.print(![])"), "false\n");

    let overridden = r#"
        class Always {
            construct new() {}
            ! {
                return "mine"
            }
        }
        System.print(!Always.new())
    "#;
    assert_eq!(run(overridden), "mine\n", "a user class may define it");
}
