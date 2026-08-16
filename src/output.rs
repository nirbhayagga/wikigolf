//! Parquet writers. The parser emits its final artifacts directly, so nothing
//! downstream has to re-read a multi-gigabyte CSV or rebuild a string map.

use anyhow::Result;
use arrow::array::{Int32Array, StringArray, UInt32Array};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

const BATCH_ROWS: usize = 1_000_000;

fn props() -> Result<WriterProperties> {
    Ok(WriterProperties::builder()
        .set_compression(Compression::ZSTD(ZstdLevel::try_new(3)?))
        .build())
}

/// Streaming writer for the `(src, dst)` edge list.
pub struct EdgeWriter {
    writer: ArrowWriter<File>,
    schema: SchemaRef,
    src: Vec<i32>,
    dst: Vec<i32>,
    pub rows: u64,
}

impl EdgeWriter {
    pub fn create(path: &Path) -> Result<Self> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("src", DataType::Int32, false),
            Field::new("dst", DataType::Int32, false),
        ]));
        let writer = ArrowWriter::try_new(File::create(path)?, schema.clone(), Some(props()?))?;
        Ok(EdgeWriter {
            writer,
            schema,
            src: Vec::with_capacity(BATCH_ROWS),
            dst: Vec::with_capacity(BATCH_ROWS),
            rows: 0,
        })
    }

    #[inline]
    pub fn push(&mut self, src: u32, dst: u32) -> Result<()> {
        self.src.push(src as i32);
        self.dst.push(dst as i32);
        self.rows += 1;
        if self.src.len() >= BATCH_ROWS {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if self.src.is_empty() {
            return Ok(());
        }
        let batch = RecordBatch::try_new(
            self.schema.clone(),
            vec![
                Arc::new(Int32Array::from(std::mem::take(&mut self.src))),
                Arc::new(Int32Array::from(std::mem::take(&mut self.dst))),
            ],
        )?;
        self.writer.write(&batch)?;
        self.src.reserve(BATCH_ROWS);
        self.dst.reserve(BATCH_ROWS);
        Ok(())
    }

    pub fn finish(mut self) -> Result<u64> {
        self.flush()?;
        self.writer.close()?;
        Ok(self.rows)
    }
}

/// Write `(id, title)` pairs — used for both articles and redirect aliases.
pub fn write_titles<'a, I>(path: &Path, id_col: &str, title_col: &str, rows: I) -> Result<u64>
where
    I: Iterator<Item = (u32, &'a str)>,
{
    let schema = Arc::new(Schema::new(vec![
        Field::new(id_col, DataType::UInt32, false),
        Field::new(title_col, DataType::Utf8, false),
    ]));
    let mut writer = ArrowWriter::try_new(File::create(path)?, schema.clone(), Some(props()?))?;

    let mut ids: Vec<u32> = Vec::with_capacity(BATCH_ROWS);
    let mut titles: Vec<&str> = Vec::with_capacity(BATCH_ROWS);
    let mut total = 0u64;

    let mut flush = |ids: &mut Vec<u32>, titles: &mut Vec<&str>| -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(UInt32Array::from(std::mem::take(ids))),
                Arc::new(StringArray::from(std::mem::take(titles))),
            ],
        )?;
        writer.write(&batch)?;
        Ok(())
    };

    for (id, title) in rows {
        ids.push(id);
        titles.push(title);
        total += 1;
        if ids.len() >= BATCH_ROWS {
            flush(&mut ids, &mut titles)?;
        }
    }
    flush(&mut ids, &mut titles)?;
    writer.close()?;
    Ok(total)
}

/// Article-to-category membership, plus the article's wikitext size.
///
/// Two outputs from one pass because they are gathered in the same place and
/// neither is worth its own traversal of a 27 GB dump.
///
/// Categories are stored as strings rather than interned ids: there are only
/// a few hundred thousand distinct ones after maintenance filtering, parquet
/// dictionary-encodes repeated values anyway, and an id table would mean a
/// second file and a join for no measurable gain.
pub struct CategoryWriter {
    writer: ArrowWriter<File>,
    schema: Arc<Schema>,
    ids: Vec<u32>,
    names: Vec<String>,
    total: u64,
}

impl CategoryWriter {
    pub fn create(path: &Path) -> Result<Self> {
        let schema = Arc::new(Schema::new(vec![
            Field::new("article_id", DataType::UInt32, false),
            Field::new("category", DataType::Utf8, false),
        ]));
        Ok(CategoryWriter {
            writer: ArrowWriter::try_new(File::create(path)?, schema.clone(), Some(props()?))?,
            schema,
            ids: Vec::with_capacity(BATCH_ROWS),
            names: Vec::with_capacity(BATCH_ROWS),
            total: 0,
        })
    }

    pub fn push(&mut self, id: u32, name: &str) -> Result<()> {
        self.ids.push(id);
        self.names.push(name.to_string());
        self.total += 1;
        if self.ids.len() >= BATCH_ROWS {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<()> {
        if self.ids.is_empty() {
            return Ok(());
        }
        let batch = RecordBatch::try_new(
            self.schema.clone(),
            vec![
                Arc::new(UInt32Array::from(std::mem::take(&mut self.ids))),
                Arc::new(StringArray::from(std::mem::take(&mut self.names))),
            ],
        )?;
        self.writer.write(&batch)?;
        Ok(())
    }

    pub fn finish(mut self) -> Result<u64> {
        self.flush()?;
        self.writer.close()?;
        Ok(self.total)
    }
}

/// Per-article wikitext byte length, indexed by dense article id.
///
/// One u32 per article — 29 MB at enwiki scale — and it is the cheapest
/// "how substantial is this article" signal available, since the dump hands
/// it over for free while the text is already in memory.
pub fn write_sizes(path: &Path, sizes: &[u32]) -> Result<u64> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::UInt32, false),
        Field::new("bytes", DataType::UInt32, false),
    ]));
    let mut writer = ArrowWriter::try_new(File::create(path)?, schema.clone(), Some(props()?))?;
    // Ids continue across chunks; restarting them at zero per batch would
    // silently give every article after the first batch the wrong size.
    for (i, chunk) in sizes.chunks(BATCH_ROWS).enumerate() {
        let base = (i * BATCH_ROWS) as u32;
        writer.write(&RecordBatch::try_new(
            schema.clone(),
            vec![
                Arc::new(UInt32Array::from_iter_values(
                    base..base + chunk.len() as u32,
                )),
                Arc::new(UInt32Array::from(chunk.to_vec())),
            ],
        )?)?;
    }
    writer.close()?;
    Ok(sizes.len() as u64)
}
