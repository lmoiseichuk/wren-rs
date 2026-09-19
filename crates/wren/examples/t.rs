fn main() {
    for path in ["language/method/name_too_long", "limit/variable_name_too_long",
                 "limit/too_many_function_parameters", "language/number/literal_too_large",
                 "language/nonlocal/undefined", "language/function/no_newline_before_close",
                 "language/list/newline_before_comma", "limit/too_many_inherited_fields"] {
        let src = std::fs::read_to_string(format!("vendor/wren/test/{path}.wren")).unwrap();
        let mut vm = wren::Vm::new();
        let r = vm.interpret(&src);
        println!("{path}: {}", match r { Ok(()) => "NO ERROR".to_string(), Err(e) => format!("err: {}", e.message()) });
    }
}
