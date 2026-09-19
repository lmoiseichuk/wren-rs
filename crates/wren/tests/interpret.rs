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
