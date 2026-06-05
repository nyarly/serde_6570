use std::collections::HashMap;

use regex::Regex;
use serde::Deserialize;
use serde_6570::{FillPolicy, ResourceMappingString, Serde6570};
use serde_json::Value;

/*
*
  {
    "level": 1,
    "variables": {
       "var"   : "value",
       "hello" : "Hello World!"
     },
     "testcases" : [
        ["{var}", "value"],
        ["'{var}'", "'value'"],
        ["{hello}", "Hello%20World%21"]
     ]
  },
*/

#[derive(Deserialize)]
struct TestCase {
    level: u64,
    variables: HashMap<String, Value>,
    testcases: Vec<(String, Value)>,
}

use table_test::table_test;

#[test]
fn conforms() {
    let testdata = include_str!("testcases/spec-examples.json");
    let testcases: HashMap<String, TestCase> = serde_json::from_str(testdata).unwrap();

    // XXX must doc: serde_6570 resource mappings must parse as a URI
    let uri_re = Regex::new("://|^/|^[{]/").expect("regex to parse");

    for (group, testcase) in testcases {
        if testcase.level > 3 {
            continue;
        }
        for (validator, mut tmpl, expected) in table_test!(testcase.testcases) {
            let vars = testcase.variables.clone();
            let mut modified_uri = false;
            if !uri_re.is_match(&tmpl) {
                tmpl = format!("/{tmpl}");
                modified_uri = true;
            }
            let rt = ResourceMappingString(tmpl.clone().into(), vec![]);
            let expanded = serde_6570::process(rt)
                .and_then(|cfg| cfg.expand(FillPolicy::Relaxed, vars))
                .and_then(|uri| Ok(uri.to_string()))
                .and_then(|s| {
                    if modified_uri {
                        Ok(s[1..].to_string())
                    } else {
                        Ok(s)
                    }
                });

            match (expanded, expected.clone()) {
                (Ok(uri), Value::String(ex)) => validator
                    .given(&format!("{group} - {tmpl}"))
                    .when("expanded")
                    .then(&format!("it should be {ex}"))
                    .assert_eq(ex, uri),
                (Ok(uri), Value::Array(ex)) => {
                    let uri_value = Value::String(uri.clone());
                    validator
                        .given(&format!("{group} - {tmpl}"))
                        .when("expanded")
                        .then(&format!("{uri_value:?} should be one of {ex:?}"))
                        .assert_eq(true, ex.iter().any(|i| i.to_string() == uri_value.clone()))
                }
                (Err(e), _) => validator
                    .given(&format!("{group} - {tmpl}"))
                    .when("processed")
                    .then("it should parse without error")
                    .custom("but:", &format!("{e:?}"))
                    .assert_eq(true, false),

                _ => {
                    panic!("testing {tmpl}, expected value was {expected} - not a string or array");
                }
            };
        }
    }
}
