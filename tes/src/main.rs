use asap_macro::asap;

#[derive(Clone)]
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

fn borrow(a: &Tracked) {
    println!("BORROW early 2 is borrowed by other function, the new 2nd");
    
}

fn moved(a: Tracked) {
    println!("MOVE early 2 is moved to other function, the new 2nd");
    
}

fn main() {
    let normal = Tracked::new("variable that uses scope based drop");
    let early  = asap!(Tracked::new("variable that uses asap drop"));

    normal.use_it();
    early.use_it();

    drop(normal);

    let early  = asap!(Tracked::new("variable that uses asap drop, the new 2nd"));
    
    let cloned = early.clone();
    borrow(&cloned);
    
    borrow(&early);
    moved(early);

    let early  = asap!(Tracked::new("variable that uses asap drop, branching"));

    if 1 == 1 {
        borrow(&early);
        
    } else {
        moved(early); 
    }

    println!("after branching");

}
