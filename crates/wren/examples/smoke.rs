fn main() {
    let programs = [
        r#"System.print(1 + 2)"#,
        r#"System.print("hello")"#,
        r#"var a = 3
System.print(a * 2)"#,
        r#"if (3 > 2) System.print("big") else System.print("small")"#,
        r#"var i = 0
while (i < 3) {
  System.print(i)
  i = i + 1
}"#,
        r#"for (i in 1..3) System.print(i)"#,
        r#"var list = [1, 2, 3]
for (x in list) System.print(x * 10)"#,
        r#"var n = 7
System.print("n is %(n) and double is %(n * 2)")"#,
        r#"System.print([1, 2, 3].count)"#,
        r#"System.print(1 == 1)
System.print("a" == "a")
System.print(1 != 2)"#,
        r#"System.print(true && false)
System.print(true || false)"#,
        r#"System.print(-5.abs)"#,
        r#"System.print(10 % 3)"#,
        r#"var t = 0
for (i in 1..10) t = t + i
System.print(t)"#,
    ];
    for source in programs {
        let mut vm = wren::Vm::new();
        println!("--- {:?}", source.lines().next().unwrap_or(""));
        match vm.interpret(source) {
            Ok(()) => print!("{}", vm.output_str()),
            Err(error) => println!("  ERROR line {}: {}", error.line(), error.message()),
        }
    }
}
