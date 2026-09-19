fn main() {
    let programs: &[(&str, &str)] = &[
        ("function literal", r#"
var add = Fn.new { |a, b| a + b }
System.print(add.call(1, 2))"#),
        ("closure captures", r#"
var make = Fn.new { |n|
  return Fn.new { |x| x + n }
}
var add5 = make.call(5)
System.print(add5.call(10))"#),
        ("shared capture", r#"
var counter = 0
var bump = Fn.new { counter = counter + 1 }
bump.call()
bump.call()
System.print(counter)"#),
        ("class + method", r#"
class Greeter {
  construct new() {}
  greet(name) { "hello " + name }
}
System.print(Greeter.new().greet("world"))"#),
        ("constructor + field", r#"
class Point {
  construct new(x, y) {
    _x = x
    _y = y
  }
  x { _x }
  y { _y }
  toString { "(%(_x), %(_y))" }
}
var p = Point.new(3, 4)
System.print(p.x)
System.print(p)"#),
        ("setter", r#"
class Box {
  construct new() { _value = 0 }
  value { _value }
  value=(v) { _value = v }
}
var b = Box.new()
b.value = 42
System.print(b.value)"#),
        ("inheritance + super", r#"
class Animal {
  construct new(name) { _name = name }
  speak { "%(_name) makes a sound" }
}
class Dog is Animal {
  construct new(name) { super(name) }
  speak { super.speak + " (woof)" }
}
System.print(Dog.new("Rex").speak)"#),
        ("static method", r#"
class Math {
  static square(n) { n * n }
}
System.print(Math.square(7))"#),
        ("operator overload", r#"
class Vec {
  construct new(x, y) {
    _x = x
    _y = y
  }
  x { _x }
  y { _y }
  +(other) { Vec.new(_x + other.x, _y + other.y) }
  toString { "(%(_x), %(_y))" }
}
System.print(Vec.new(1, 2) + Vec.new(10, 20))"#),
        ("is operator", r#"
class A { construct new() {} }
class B is A { construct new() { super() } }
System.print(B.new() is A)
System.print(1 is Num)
System.print("x" is Num)"#),
        ("recursion", r#"
class Fib {
  static get(n) {
    if (n < 2) return n
    return get(n - 1) + get(n - 2)
  }
}
System.print(Fib.get(20))"#),
        ("subscript operator", r#"
class Grid {
  construct new() { _cells = [1, 2, 3] }
  [i] { _cells[i] }
  [i]=(v) { _cells[i] = v }
}
var g = Grid.new()
g[1] = 99
System.print(g[1])"#),
        ("block argument", r#"
class Each {
  static run(list, fn) {
    for (x in list) fn.call(x)
  }
}
Each.run([1, 2, 3]) { |x| System.print(x * 2) }"#),
    ];

    let mut failures = 0;
    for (label, source) in programs {
        let mut vm = wren::Vm::new();
        match vm.interpret(source) {
            Ok(()) => println!("--- {label}\n{}", vm.output_str()),
            Err(error) => {
                failures += 1;
                println!("--- {label}\n  ERROR line {}: {}\n", error.line(), error.message());
            }
        }
    }
    println!("{failures} of {} failed", programs.len());
}
