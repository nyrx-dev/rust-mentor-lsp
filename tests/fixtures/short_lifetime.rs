fn main() {
    let reference;
    {
        let value = String::from("hello");
        reference = &value;
    }
    println!("{}", reference);
}
