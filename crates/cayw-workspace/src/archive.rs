use crate::{Context, Workspace, failure, filesystem as fs, io_error, parse};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use cayw_codemode::CapabilityError;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{Cursor, Read},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Args {
    path: String,
    format: Format,
    entry: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Format {
    Zip,
    Tar,
}
#[derive(Serialize)]
struct Entry {
    path: String,
    kind: &'static str,
    bytes: u64,
    compressed_bytes: u64,
}
struct Names {
    kinds: BTreeMap<String, bool>,
}
impl Names {
    fn new() -> Self {
        Self {
            kinds: BTreeMap::new(),
        }
    }
    fn add(
        &mut self,
        name: &str,
        directory: bool,
        workspace: &Workspace,
    ) -> Result<String, CapabilityError> {
        let name = if directory {
            name.strip_suffix('/').unwrap_or(name)
        } else {
            name
        };
        let parts = fs::components(name, false)?;
        if parts.len() > workspace.limits.max_depth {
            return Err(failure("archive entry depth limit exceeded"));
        }
        let key = name.to_lowercase();
        if self.kinds.insert(key, directory).is_some() {
            return Err(failure("duplicate or case-alias archive entry"));
        }
        Ok(name.into())
    }
    fn finish(&self) -> Result<(), CapabilityError> {
        for name in self.kinds.keys() {
            let parts: Vec<_> = name.split('/').collect();
            let mut prefix = String::new();
            for part in &parts[..parts.len().saturating_sub(1)] {
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(part);
                if self.kinds.get(&prefix) == Some(&false) {
                    return Err(failure("archive entry has a non-directory ancestor"));
                }
            }
        }
        Ok(())
    }
}
fn expand(
    workspace: &Workspace,
    total: &mut u64,
    size: u64,
    compressed: u64,
) -> Result<(), CapabilityError> {
    if size > workspace.limits.max_file_bytes as u64
        || size > compressed.saturating_mul(workspace.limits.max_compression_ratio)
    {
        return Err(failure(
            "archive entry byte or compression ratio limit exceeded",
        ));
    }
    *total = total
        .checked_add(size)
        .ok_or_else(|| failure("archive size overflow"))?;
    if *total > workspace.limits.max_archive_expanded_bytes as u64 {
        return Err(failure("archive total expanded byte limit exceeded"));
    }
    Ok(())
}
fn encoded(bytes: Vec<u8>) -> Value {
    let count = bytes.len();
    let sha256 = fs::hash(&bytes);
    match String::from_utf8(bytes) {
        Ok(content) => json!({"encoding":"utf8","content":content,"bytes":count,"sha256":sha256}),
        Err(error) => {
            json!({"encoding":"base64","content":STANDARD.encode(error.into_bytes()),"bytes":count,"sha256":sha256})
        }
    }
}
pub(crate) fn execute(
    workspace: &Workspace,
    name: &str,
    args: &Value,
    context: &Context,
) -> Result<Value, CapabilityError> {
    let args: Args = parse(args)?;
    if (name == "archive.read") != args.entry.is_some() {
        return Err(failure(
            "archive.read requires entry; archive.list forbids it",
        ));
    }
    if let Some(entry) = &args.entry {
        fs::components(entry, false)?;
    }
    let parts = fs::components(&args.path, false)?;
    let (dir, filename) = fs::parent(&workspace.root, &parts)?;
    // The compressed/container input is bounded before any parser allocates an
    // entry table. Only the chosen entry is decompressed, never archive nesting.
    let bytes = fs::read(&dir, filename, workspace.limits.max_archive_bytes, context)?;
    let archive_sha256 = fs::hash(&bytes);
    let (entries, content) = match args.format {
        Format::Zip => zip(workspace, bytes, args.entry.as_deref(), context)?,
        Format::Tar => tar(workspace, bytes, args.entry.as_deref(), context)?,
    };
    if let Some(content) = content {
        Ok(
            json!({"path":args.path,"archive_sha256":archive_sha256,"entry":args.entry,"data":encoded(content)}),
        )
    } else if args.entry.is_some() {
        Err(failure("archive entry is absent or is a directory"))
    } else {
        Ok(json!({"path":args.path,"archive_sha256":archive_sha256,"entries":entries}))
    }
}
fn u16_at(bytes: &[u8], offset: usize) -> Result<u16, CapabilityError> {
    Ok(u16::from_le_bytes(
        bytes
            .get(offset..offset + 2)
            .ok_or_else(|| failure("truncated ZIP integer"))?
            .try_into()
            .expect("two bytes"),
    ))
}
fn u32_at(bytes: &[u8], offset: usize) -> Result<u32, CapabilityError> {
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .ok_or_else(|| failure("truncated ZIP integer"))?
            .try_into()
            .expect("four bytes"),
    ))
}
fn slice(bytes: &[u8], start: usize, size: usize) -> Result<&[u8], CapabilityError> {
    bytes
        .get(
            start
                ..start
                    .checked_add(size)
                    .ok_or_else(|| failure("ZIP range overflow"))?,
        )
        .ok_or_else(|| failure("ZIP range out of bounds"))
}
fn extras(bytes: &[u8]) -> Result<(), CapabilityError> {
    let mut at = 0;
    while at < bytes.len() {
        let id = u16_at(bytes, at)?;
        let size = u16_at(bytes, at + 2)? as usize;
        slice(bytes, at + 4, size)?;
        // Only metadata without link semantics is accepted. ZIP64, ASi Unix,
        // old Unix link extras and unsupported extension records are rejected.
        if !matches!(id, 0x5455 | 0x7875 | 0x000a) {
            return Err(failure("unsupported ZIP extra field"));
        }
        at += 4 + size;
    }
    Ok(())
}
fn zip_table(
    workspace: &Workspace,
    bytes: &[u8],
    context: &Context,
) -> Result<Vec<Entry>, CapabilityError> {
    if bytes.len() < 22 {
        return Err(failure("truncated ZIP"));
    }
    let lower = bytes.len().saturating_sub(65_557);
    let eocd = (lower..=bytes.len() - 22)
        .rev()
        .find(|at| {
            bytes.get(*at..*at + 4) == Some(b"PK\x05\x06".as_slice())
                && u16_at(bytes, *at + 20)
                    .is_ok_and(|comment| *at + 22 + comment as usize == bytes.len())
        })
        .ok_or_else(|| failure("ZIP end record missing"))?;
    let count = u16_at(bytes, eocd + 10)? as usize;
    let cd_bytes = u32_at(bytes, eocd + 12)? as usize;
    let cd_start = u32_at(bytes, eocd + 16)? as usize;
    if u16_at(bytes, eocd + 4)? != 0
        || u16_at(bytes, eocd + 6)? != 0
        || u16_at(bytes, eocd + 8)? as usize != count
        || count == 65_535
        || count > workspace.limits.max_archive_entries
        || cd_start.checked_add(cd_bytes) != Some(eocd)
    {
        return Err(failure(
            "ZIP entry limit, ZIP64, split archive or invalid directory",
        ));
    }
    let mut at = cd_start;
    let mut entries = vec![];
    let mut ranges = vec![];
    let mut names = Names::new();
    let mut total = 0;
    for _ in 0..count {
        context.check()?;
        if slice(bytes, at, 4)? != b"PK\x01\x02" {
            return Err(failure("invalid ZIP directory header"));
        }
        slice(bytes, at, 46)?;
        let flags = u16_at(bytes, at + 8)?;
        let method = u16_at(bytes, at + 10)?;
        let compressed = u32_at(bytes, at + 20)? as usize;
        let size = u32_at(bytes, at + 24)? as u64;
        let name_size = u16_at(bytes, at + 28)? as usize;
        let extra_size = u16_at(bytes, at + 30)? as usize;
        let comment_size = u16_at(bytes, at + 32)? as usize;
        if flags & !0x0806 != 0
            || !matches!(method, 0 | 8)
            || (method == 0 && flags & 6 != 0)
            || u16_at(bytes, at + 34)? != 0
            || compressed == u32::MAX as usize
            || size == u32::MAX as u64
        {
            return Err(failure(
                "unsupported ZIP flags, compression, encryption, descriptor or ZIP64",
            ));
        }
        let raw = slice(bytes, at + 46, name_size)?;
        let name = std::str::from_utf8(raw).map_err(|_| failure("ZIP name must be UTF-8"))?;
        let directory = name.ends_with('/');
        let made_by = bytes[at + 5];
        if !matches!(made_by, 0 | 3 | 10) {
            return Err(failure("unsupported ZIP creator platform"));
        }
        let attributes = u32_at(bytes, at + 38)?;
        if attributes & 0x40 != 0 {
            return Err(failure("ZIP device entry is forbidden"));
        }
        let mode = if made_by == 3 { attributes >> 16 } else { 0 };
        let file_type = mode & 0o170000;
        if (file_type != 0 && file_type != if directory { 0o040000 } else { 0o100000 })
            || (!directory && attributes & 0x10 != 0)
            || (directory && size != 0)
        {
            return Err(failure(
                "ZIP symbolic link, special entry or inconsistent type",
            ));
        }
        let path = names.add(name, directory, workspace)?;
        extras(slice(bytes, at + 46 + name_size, extra_size)?)?;
        let local = u32_at(bytes, at + 42)? as usize;
        if slice(bytes, local, 4)? != b"PK\x03\x04" {
            return Err(failure("invalid ZIP local header"));
        }
        slice(bytes, local, 30)?;
        let local_name_size = u16_at(bytes, local + 26)? as usize;
        let local_extra_size = u16_at(bytes, local + 28)? as usize;
        if u16_at(bytes, local + 6)? != flags
            || u16_at(bytes, local + 8)? != method
            || slice(bytes, local + 14, 12)? != slice(bytes, at + 16, 12)?
            || slice(bytes, local + 30, local_name_size)? != raw
        {
            return Err(failure("ZIP local/central header mismatch"));
        }
        extras(slice(
            bytes,
            local + 30 + local_name_size,
            local_extra_size,
        )?)?;
        let data = local
            .checked_add(30 + local_name_size + local_extra_size)
            .ok_or_else(|| failure("ZIP offset overflow"))?;
        let end = data
            .checked_add(compressed)
            .ok_or_else(|| failure("ZIP payload overflow"))?;
        if end > cd_start {
            return Err(failure("ZIP payload overlaps directory"));
        }
        if method == 0 && compressed as u64 != size {
            return Err(failure("ZIP stored size mismatch"));
        }
        expand(workspace, &mut total, size, compressed as u64)?;
        ranges.push((local, end));
        entries.push(Entry {
            path,
            kind: if directory { "directory" } else { "file" },
            bytes: size,
            compressed_bytes: compressed as u64,
        });
        at = at
            .checked_add(46 + name_size + extra_size + comment_size)
            .ok_or_else(|| failure("ZIP directory overflow"))?;
        if at > eocd {
            return Err(failure("ZIP directory overrun"));
        }
    }
    if at != eocd {
        return Err(failure("ZIP directory length mismatch"));
    }
    ranges.sort_unstable();
    if ranges.windows(2).any(|range| range[0].1 > range[1].0) {
        return Err(failure("overlapping ZIP entries"));
    }
    names.finish()?;
    Ok(entries)
}
fn zip(
    workspace: &Workspace,
    bytes: Vec<u8>,
    target: Option<&str>,
    context: &Context,
) -> Result<(Vec<Entry>, Option<Vec<u8>>), CapabilityError> {
    // Validate raw records before ZipArchive can merge duplicate names or allocate
    // from an untrusted advertised entry count.
    let entries = zip_table(workspace, &bytes, context)?;
    let Some(target) = target else {
        return Ok((entries, None));
    };
    let Some(index) = entries
        .iter()
        .position(|entry| entry.path == target && entry.kind == "file")
    else {
        return Ok((entries, None));
    };
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).map_err(|error| failure(error.to_string()))?;
    context.check()?;
    if archive.len() != entries.len() {
        return Err(failure("ZIP parser entry count mismatch"));
    }
    let mut file = archive
        .by_index(index)
        .map_err(|error| failure(error.to_string()))?;
    let content = fs::read_bounded(&mut file, entries[index].bytes as usize, context)?;
    if content.len() as u64 != entries[index].bytes {
        return Err(failure("ZIP actual size mismatch"));
    }
    Ok((entries, Some(content)))
}
fn tar(
    workspace: &Workspace,
    bytes: Vec<u8>,
    target: Option<&str>,
    context: &Context,
) -> Result<(Vec<Entry>, Option<Vec<u8>>), CapabilityError> {
    scan_tar(workspace, Cursor::new(bytes), target, context)
}

fn scan_tar(
    workspace: &Workspace,
    reader: impl Read,
    target: Option<&str>,
    context: &Context,
) -> Result<(Vec<Entry>, Option<Vec<u8>>), CapabilityError> {
    struct CheckedReader<'a, R> {
        reader: R,
        context: &'a Context,
    }
    impl<R: Read> Read for CheckedReader<'_, R> {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.context.check().map_err(std::io::Error::other)?;
            let size = buffer.len().min(8192);
            let read = self.reader.read(&mut buffer[..size])?;
            self.context.check().map_err(std::io::Error::other)?;
            Ok(read)
        }
    }
    let mut archive = tar::Archive::new(CheckedReader { reader, context });
    archive.set_ignore_zeros(true);
    let iterator = archive.entries().map_err(io_error)?.raw(true);
    let mut entries = vec![];
    let mut names = Names::new();
    let mut total = 0;
    let mut content = None;
    for entry in iterator {
        context.check()?;
        if entries.len() >= workspace.limits.max_archive_entries {
            return Err(failure("TAR entry limit exceeded"));
        }
        let mut entry = entry.map_err(io_error)?;
        let header = entry.header();
        let raw = header.as_bytes();
        // Use raw header fields: tar path accessors normalize backslashes.
        let type_byte = raw[156];
        let directory = match type_byte {
            b'0' | 0 => false,
            b'5' => true,
            _ => {
                return Err(failure(
                    "TAR links, devices, sparse and extension entries are unsupported",
                ));
            }
        };
        let field = |bytes: &[u8]| {
            let end = bytes
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(bytes.len());
            std::str::from_utf8(&bytes[..end])
                .map(str::to_owned)
                .map_err(|_| failure("TAR name must be UTF-8"))
        };
        let name = field(&raw[..100])?;
        let prefix = if &raw[257..263] == b"ustar\0" {
            field(&raw[345..500])?
        } else {
            String::new()
        };
        let full = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let path = names.add(&full, directory, workspace)?;
        let size = header.size().map_err(io_error)?;
        if directory && size != 0 {
            return Err(failure("TAR directory has payload"));
        }
        expand(workspace, &mut total, size, size)?;
        if target == Some(path.as_str()) && !directory {
            let data = fs::read_bounded(&mut entry, size as usize, context)?;
            if data.len() as u64 != size {
                return Err(failure("truncated TAR payload"));
            }
            content = Some(data);
        } else {
            // Read bounded chunks to detect truncation and cancellation, even for
            // unselected entries. No extraction or extension interpretation.
            let mut count = 0u64;
            let mut chunk = [0; 8192];
            loop {
                context.check()?;
                let read = entry.read(&mut chunk).map_err(io_error)?;
                if read == 0 {
                    break;
                }
                count += read as u64;
                if count > size {
                    return Err(failure("TAR payload exceeds header size"));
                }
            }
            if count != size {
                return Err(failure("truncated TAR payload"));
            }
        }
        entries.push(Entry {
            path,
            kind: if directory { "directory" } else { "file" },
            bytes: size,
            compressed_bytes: size,
        });
    }
    context.check()?;
    names.finish()?;
    context.check()?;
    Ok((entries, content))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CancellationToken, Limits};
    use std::time::{Duration, Instant};

    struct InterruptedZeros {
        bytes: Cursor<Vec<u8>>,
        reads: usize,
        interrupt: Box<dyn FnMut()>,
    }
    impl Read for InterruptedZeros {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            self.reads += 1;
            if self.reads == 4 {
                (self.interrupt)();
            }
            self.bytes.read(buffer)
        }
    }

    #[test]
    fn tar_zero_headers_cannot_hide_cancellation_inside_the_parser() {
        let root = tempfile::tempdir().unwrap();
        let workspace =
            Workspace::open(root.path().canonicalize().unwrap(), Limits::default()).unwrap();
        let cancel = CancellationToken::new();
        let context = Context::new(cancel.clone(), Duration::from_secs(5)).unwrap();
        let reader = InterruptedZeros {
            bytes: Cursor::new(vec![0; 8192]),
            reads: 0,
            interrupt: Box::new(move || cancel.cancel()),
        };
        let error = scan_tar(&workspace, reader, None, &context)
            .err()
            .expect("cancelled zero headers must fail");
        assert!(!error.outcome_unknown);
        assert!(error.message.contains("cancelled"));
    }

    #[test]
    fn tar_zero_headers_cannot_hide_an_elapsed_deadline_inside_the_parser() {
        let root = tempfile::tempdir().unwrap();
        let workspace =
            Workspace::open(root.path().canonicalize().unwrap(), Limits::default()).unwrap();
        let context = Context::new(CancellationToken::new(), Duration::from_millis(20)).unwrap();
        let deadline = context.deadline;
        let reader = InterruptedZeros {
            bytes: Cursor::new(vec![0; 8192]),
            reads: 0,
            interrupt: Box::new(move || {
                while let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
                    std::thread::sleep(remaining);
                }
            }),
        };
        let error = scan_tar(&workspace, reader, None, &context)
            .err()
            .expect("expired zero headers must fail");
        assert!(!error.outcome_unknown);
        assert!(error.message.contains("deadline"));
    }
}
