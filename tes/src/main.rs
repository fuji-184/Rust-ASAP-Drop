use asap_macro::asap;

struct Tracked {
    name: &'static str,
}

impl Tracked {
    fn new(name: &'static str) -> Self {
        println!("CREATE {name}");
        Tracked { name }
    }

    fn use_it(&self) {
        println!("USE    {}", self.name);
    }
}

impl Drop for Tracked {
    fn drop(&mut self) {
        println!("DROP   {}", self.name);
    }
}

fn main() {
    let normal = Tracked::new("normal");
    let early  = asap!(Tracked::new("early"));

    normal.use_it();
    early.use_it();

    normal.use_it();

    drop(normal);
}
