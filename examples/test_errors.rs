#[allow(dead_code)]
fn test_use_of_moved_value() {
    let s = String::from("hello");
    println!("{}", s);
    let s2 = s;
    println!("{}", s2);
}

#[allow(dead_code)]
fn test_simultaneous_mutable_and_immutable_borrow() {
    let mut data = vec![1, 2, 3];
    data.push(4);
    let first = &data[0];
    println!("{}", first);
}

#[allow(dead_code)]
fn test_borrowed_value_does_not_live_long_enough() {
    let x = 5;
    let r = &x;
    println!("{}", r);
}

fn main() {
    test_simultaneous_mutable_and_immutable_borrow();
}
