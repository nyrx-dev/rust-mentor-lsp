fn main() {
    let value = String::from("hello");
    let moved = value;
    println!("{} {}", value, moved);
}
