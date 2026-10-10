fn main() {
    let names: Vec<_> = chrono_tz::TZ_VARIANTS.iter().map(|tz| tz.name()).collect();
    println!("{}", serde_json::json!(names));
}
