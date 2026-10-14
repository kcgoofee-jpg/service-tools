use gate_core::jsondump;
use gate_golden::{assert_eq_py, load};
use serde_json::json;

const M: &str = env!("CARGO_MANIFEST_DIR");

#[test]
fn dumps_golden() {
    let cases = load(M, "jsondump/dumps");
    assert!(cases.len() > 1000);
    for c in cases {
        let got = jsondump::dumps(c.arg(0));
        assert_eq_py(&c, &json!(got), c.out());
    }
}
