use std::sync::{Arc, Mutex};

fn main() {
    let a = Ref::new();
    let b = a.clone();
    drop(a);
    drop(b);
}

struct Inner {}
#[derive(Clone)]
struct Ref(Arc<Mutex<Inner>>);
impl Ref {
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(Inner {})))
    }
}
impl Drop for Ref {
    fn drop(&mut self) {
        eprintln!("drop! ref count {}", Arc::strong_count(&self.0));
    }
}
