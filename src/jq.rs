pub mod jv;

use jq_sys::{jq_compile, jq_init, jq_next, jq_start, jq_state, jq_teardown};
use jv::{JVKind, JV};
use serde_json::{value::Value, Deserializer};
use std::ffi::CString;

pub fn run_jq_query(content: &[Value], prog: &mut jq_rs::JqProgram) -> Vec<Value> {
    let right_strings: Vec<String> = content
        .iter()
        .map(|j| prog.run(&j.to_string()).expect("jq execution error"))
        .collect();
    let right_content: Result<Vec<Value>, _> = right_strings
        .iter()
        .flat_map(|j| Deserializer::from_str(j).into_iter::<Value>())
        .collect();
    right_content.expect("json decoding error")
}

pub struct JQ {
    ptr: *mut jq_state,
}

impl Drop for JQ {
    fn drop(&mut self) {
        unsafe { jq_teardown(&mut self.ptr) };
    }
}

impl JQ {
    fn new() -> Self {
        JQ {
            ptr: unsafe { jq_init() },
        }
    }
    pub fn compile(s: &str) -> Option<Self> {
        let prog = JQ::new();
        let cstr = CString::new(s).expect("Nul byte in jq program");
        let ok = unsafe { jq_compile(prog.ptr, cstr.as_ptr()) };
        if ok > 0 {
            Some(prog)
        } else {
            None
        }
    }
    pub fn execute(&mut self, input: JV) -> impl Iterator<Item = JV> + '_ {
        unsafe { jq_start(self.ptr, input.unwrap_without_drop(), 0) };
        JQResults { jq: self }
    }
}

struct JQResults<'a> {
    jq: &'a mut JQ,
}

impl<'a> Iterator for JQResults<'a> {
    type Item = JV;
    fn next(&mut self) -> Option<Self::Item> {
        let raw_res = unsafe { jq_next(self.jq.ptr) };
        let res = JV { ptr: raw_res };
        match res.get_kind() {
            JVKind::Invalid => None,
            _ => Some(res),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{JQ, JV};
    use proptest::{prelude::*, proptest};
    use serde_json::value::Value;
    fn arb_json() -> impl Strategy<Value = Value> {
        let leaf = prop_oneof![
            Just(Value::Null),
            any::<bool>().prop_map(Value::Bool),
            any::<f64>().prop_map(|f| f.into()),
            ".*".prop_map(Value::String),
        ];
        leaf.prop_recursive(
            8,   // 8 levels deep
            256, // Shoot for maximum size of 256 nodes
            10,  // We put up to 10 items per collection
            |inner| {
                prop_oneof![
                    // Take the inner strategy and make the two recursive cases.
                    prop::collection::vec(inner.clone(), 0..10).prop_map(Value::Array),
                    prop::collection::hash_map(".*", inner, 0..10)
                        .prop_map(|m| { Value::Object(m.into_iter().collect()) }),
                ]
            },
        )
    }
    proptest! {
        #[test]
        fn prop_jq_roundtrip(value in arb_json()) {
            let jv = JV::from_serde(&value);
            let mut jq = JQ::compile(".").unwrap();
            let results : Vec<Value> = jq.execute(jv).map(|jv| jv.to_serde().unwrap()).collect();
            assert_eq!(vec![value], results);
        }
    }
}
