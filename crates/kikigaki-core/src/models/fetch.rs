use std::io::Write;

/// Streams a URL into a caller-owned destination.
pub trait Fetcher {
    /// Downloads `url`, reporting the cumulative byte count after each chunk.
    fn fetch(
        &self,
        url: &str,
        dest: &mut dyn Write,
        progress: &mut dyn FnMut(u64),
    ) -> anyhow::Result<()>;
}

/// File-backed fetcher used by tests and integration fixtures.
#[cfg(test)]
#[derive(Debug, Clone)]
pub struct FileFetcher {
    /// Directory containing files named after URL path segments.
    pub root: std::path::PathBuf,
}

#[cfg(test)]
impl Fetcher for FileFetcher {
    fn fetch(
        &self,
        url: &str,
        dest: &mut dyn Write,
        progress: &mut dyn FnMut(u64),
    ) -> anyhow::Result<()> {
        use anyhow::Context;
        use std::io::Read;

        let name = url
            .rsplit('/')
            .next()
            .filter(|name| !name.is_empty())
            .context("download URL has no final path segment")?;
        let mut source = std::fs::File::open(self.root.join(name))
            .with_context(|| format!("open test download {name}"))?;
        let mut buffer = [0_u8; 64 * 1024];
        let mut total = 0_u64;
        loop {
            let read = source.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            dest.write_all(&buffer[..read])?;
            total += read as u64;
            progress(total);
        }
        Ok(())
    }
}
