#![allow(non_upper_case_globals, non_camel_case_types, non_snake_case, dead_code)]
pub mod ffi {
    include!(concat!(env!("OUT_DIR"), "/vpx_bindings.rs"));
}
