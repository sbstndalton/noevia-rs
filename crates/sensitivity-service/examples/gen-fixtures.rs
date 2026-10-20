#[path = "../tests/support/oracle.rs"]
mod oracle;
fn main() -> Result<(), oracle::Error> {
    let mut args = std::env::args().skip(1);
    let source = args.next().ok_or("source path required")?;
    let target = args.next().ok_or("output path required")?;
    oracle::generate(std::path::Path::new(&source), std::path::Path::new(&target))
}
