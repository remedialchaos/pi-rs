fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let input = args.next().ok_or("missing input")?;
    let output = args.next().ok_or("missing output")?;
    let core = std::fs::read(input)?;
    let component = wit_component::ComponentEncoder::default()
        .module(&core)?
        .validate(true)
        .encode()?;
    Ok(std::fs::write(output, component)?)
}
