fn main() {
    let src = r#"
var a = {"one": 1, "two": 2, "three": 3, "four": 4}
System.print(a.count)
System.print(a.iterate(null))
System.print(a.iterate(0))
System.print(a.iterate(1))
System.print(a.iterate(2))
System.print(a.iterate(3))
"#;
    let mut vm = wren::Vm::new();
    match vm.interpret(src) {
        Ok(()) => print!("{}", vm.output_str()),
        Err(e) => println!("ERR line {}: {}", e.line(), e.message()),
    }
}
