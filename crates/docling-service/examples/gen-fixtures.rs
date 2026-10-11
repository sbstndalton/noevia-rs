#[path = "../tests/support/oracle.rs"]
mod oracle;
fn main() -> Result<(), oracle::Error> {
    let mut args = std::env::args_os().skip(1);
    let source = args.next().ok_or("required merged Docling source")?;
    let output = args.next().ok_or("required fixture output")?;
    if args.next().is_some() {
        return Err("unexpected argument".into());
    }
    oracle::generate(std::path::Path::new(&source), std::path::Path::new(&output))
}
