macro_rules! answer {
    () => {
        42
    };
    ($x:expr) => {
        $x
    };
}

macro twice {
    ($e:expr) => {
        $e + $e
    };
}

fn uses_macros() {
    let _a = answer!();
    let _b = answer!(7);
    println!("{}", twice!(3));
}
