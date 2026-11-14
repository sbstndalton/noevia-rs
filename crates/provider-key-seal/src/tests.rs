use super::*;

fn s(v: &str) -> JValue {
    JValue::Str(v.to_owned())
}

#[test]
fn plaintext_is_a_non_empty_string_that_is_not_an_envelope() {
    assert!(is_plaintext_key(&s("synthetic-plain-key")));
    assert!(is_plaintext_key(&s("enc:")));
    assert!(is_plaintext_key(&s("enc:v")));
    assert!(is_plaintext_key(&s("enc:v:abc")), "no digit");
    assert!(is_plaintext_key(&s("enc:vx1:abc")));
    assert!(is_plaintext_key(&s("enc:v1")), "no colon after the digits");
    assert!(is_plaintext_key(&s(" enc:v1:abc")), "anchored at the start");
    assert!(is_plaintext_key(&s("ENC:v1:abc")), "case-sensitive");
    assert!(
        is_plaintext_key(&s("enc:v١:abc")),
        "\\d is ASCII only in JS"
    );
    assert!(!is_plaintext_key(&s("enc:v1:")));
    assert!(
        !is_plaintext_key(&s("enc:v2:AAAA")),
        "a bound envelope is left alone (L5)"
    );
    assert!(!is_plaintext_key(&s("enc:v10:x")));
    assert!(!is_plaintext_key(&s("")));
    assert!(!is_plaintext_key(&JValue::Num(12345.0)));
    assert!(!is_plaintext_key(&JValue::Null));
    assert!(!is_plaintext_key(&JValue::Bool(true)));
    assert!(!is_plaintext_key(&JValue::Undefined));
    assert!(!is_plaintext_key(&JValue::Arr(vec![s("x")])));
}
