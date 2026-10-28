use std::fs::File;
use std::io;

fn main() -> io::Result<()> {
    let mut client = File::open("/scheme/chan/hello")?;
    io::copy(&mut client, &mut io::stdout())?;

    Ok(())
}
