#![allow(dead_code)]
#![forbid(unsafe_code)]

//! Inner docs after inner attributes.

fn uses_inner() {
    #![allow(unused_variables)]
    let x = 1;
}
