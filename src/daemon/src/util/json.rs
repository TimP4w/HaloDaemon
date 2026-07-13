// SPDX-License-Identifier: GPL-3.0-or-later
//! Structural bounds on untrusted JSON: caps nesting depth and total collection
//! nodes so a small document can't expand into excessive nested work.

use serde_json::Value;

pub fn check_bounds(v: &Value, max_depth: usize, max_nodes: usize) -> Result<(), String> {
    fn walk(
        v: &Value,
        depth: usize,
        nodes: &mut usize,
        max_depth: usize,
        max_nodes: usize,
    ) -> Result<(), String> {
        if depth > max_depth {
            return Err(format!("nesting exceeds {max_depth}"));
        }
        match v {
            Value::Array(a) => {
                *nodes += a.len();
                if *nodes > max_nodes {
                    return Err(format!("collection nodes exceed {max_nodes}"));
                }
                for e in a {
                    walk(e, depth + 1, nodes, max_depth, max_nodes)?;
                }
            }
            Value::Object(o) => {
                *nodes += o.len();
                if *nodes > max_nodes {
                    return Err(format!("collection nodes exceed {max_nodes}"));
                }
                for e in o.values() {
                    walk(e, depth + 1, nodes, max_depth, max_nodes)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    walk(v, 0, &mut 0, max_depth, max_nodes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rejects_deep_and_wide_but_allows_big_string() {
        let mut deep = json!(0);
        for _ in 0..10 {
            deep = json!([deep]);
        }
        assert!(check_bounds(&deep, 5, 100).is_err());
        assert!(check_bounds(&json!(vec![0u8; 101]), 5, 100).is_err());
        assert!(check_bounds(&json!({"d": "x".repeat(10_000)}), 5, 100).is_ok());
    }
}
