use chrono::DateTime;
use clap::{App, Arg, SubCommand};
use glob::glob;
use serde::{de, Deserialize, Deserializer, Serialize};
use skim::prelude::*;
use skim::MatchEngineFactory;
use std::io::Cursor;
use std::io::{Error, ErrorKind};
use std::{ffi::OsString, fmt, fs, io, io::Read, marker::PhantomData, path::Path};
use tantivy::{collector::TopDocs, LeasedItem, Searcher, doc, query::QueryParser, schema::*, Index};
use toml::Value as tomlVal;
use yaml_rust::YamlEmitter;

// TODO
// emit only filename by default with option to emit JSON
// Pull in skim style dynamic prompting reloading

#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct Doc {
    #[serde(default)]
    author: String,
    #[serde(skip_deserializing)]
    full_path: OsString,
    #[serde(skip_deserializing)]
    body: String,
    date: String,
    #[serde(default)]
    filename: String,
    #[serde(deserialize_with = "string_or_list_string")]
    tags: Vec<String>,
    title: String,
}

// Support Deserializing a string into a list of string of length 1
fn string_or_list_string<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    struct StringOrVec(PhantomData<Vec<String>>);

    impl<'de> de::Visitor<'de> for StringOrVec {
        type Value = Vec<String>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("string or list of strings")
        }

        // Value is a single string: return a Vec containing that single string
        fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(vec![value.to_owned()])
        }

        fn visit_seq<S>(self, visitor: S) -> Result<Self::Value, S::Error>
        where
            S: de::SeqAccess<'de>,
        {
            Deserialize::deserialize(de::value::SeqAccessDeserializer::new(visitor))
        }
    }

    deserializer.deserialize_any(StringOrVec(PhantomData))
}

fn main() -> tantivy::Result<()> {
    color_backtrace::install();

    let default_config_file = shellexpand::tilde("~/.config/tika/tika.toml");

    let cli = App::new("tika")
        .version("1.0")
        .author("Steve <steve@little-fluffy.cloud>")
        .about("Things I Know About: Zettlekasten-like Markdown+FrontMatter Indexer and query tool")
        .arg(
            Arg::with_name("config")
                .short("c")
                .value_name("FILE")
                .help(
                    format!(
                        "Point to a config TOML file, defaults to `{}`",
                        default_config_file
                    )
                    .as_str(),
                )
                .default_value(&default_config_file)
                .takes_value(true),
        )
        .arg(
            Arg::with_name("source")
                .short("s")
                .value_name("DIRECTORY")
                .help("Glob path to markdown files to load")
                .takes_value(true),
        )
        .subcommand(
            SubCommand::with_name("query")
                .about("Query the index")
                .arg(Arg::with_name("query").required(true).help("Query string")),
        )
        .get_matches();

    // Define and build the Index Schema
    let mut schema_builder = Schema::builder();

    let author = schema_builder.add_text_field("author", TEXT);
    let body = schema_builder.add_text_field("body", TEXT);
    let date = schema_builder.add_date_field("date", INDEXED | STORED);
    let filename = schema_builder.add_text_field("filename", TEXT | STORED);
    let full_path = schema_builder.add_text_field("full_path", TEXT | STORED);
    let tags = schema_builder.add_text_field("tags", TEXT | STORED);
    let title = schema_builder.add_text_field("title", TEXT | STORED);
    let schema = schema_builder.build();

    let index = Index::create_in_ram(schema.clone());
    let mut index_writer = index.writer(100_000_000).unwrap();

    let cfg_file = cli.value_of("config").unwrap();
    let cfg_fh = fs::OpenOptions::new()
        .read(true)
        .write(false)
        .create(false)
        .open(cfg_file)?;
    let mut buf_reader = io::BufReader::new(cfg_fh);
    let mut contents = String::new();
    buf_reader.read_to_string(&mut contents)?;
    let toml_contents = contents.parse::<tomlVal>().unwrap();

    let source_glob = toml_contents
        .get("source-glob")
        .expect("Failed to find 'source-glob' heading in toml config")
        .as_str()
        .expect("Error taking source-glob value as string");

    let source = cli.value_of("source").unwrap_or(source_glob);
    let glob_path = Path::new(&source);
    let glob_str = shellexpand::tilde(glob_path.to_str().unwrap());

    println!("Sourcing Markdown documents matching : {}", glob_str);

    for entry in glob(&glob_str).expect("Failed to read glob pattern") {
        match entry {
            Ok(path) => {
                if let Ok(doc) = index_file(&path) {
                    let rfc3339 = DateTime::parse_from_rfc3339(&doc.date).unwrap();
                    let thingit = rfc3339.with_timezone(&chrono::Utc);
                    let thedate = Value::Date(thingit);

                    let f = path.to_str().unwrap();
                    index_writer.add_document(doc!(
                        author => doc.author,
                        body => doc.body,
                        date => thedate,
                        filename => doc.filename,
                        full_path => f,
                        tags => doc.tags.join(" "),
                        title => doc.title,
                    ));
                    println!("✅ {}", f);
                } else {
                    println!("Failed to read path {}", path.display());
                }
            }

            Err(e) => println!("{:?}", e),
        }
    }

    index_writer.commit().unwrap();

    let reader = index.reader()?;
    let searcher = reader.searcher();
    let query_parser = QueryParser::for_index(&index, vec![author, body, filename, tags, title]);

    if let Some(cli) = cli.subcommand_matches("query") {
        let query = cli.value_of("query").unwrap();
        println!("Query {}", query);

        //let query = query_parser.parse_query("vim")?;
        //let query = query_parser.parse_query("tags:kubernetes")?;
        //let query = query_parser.parse_query("date:2020-07-24T13:03:50-04:00")?;
        //let query = query_parser.parse_query("* AND date:\"2019-04-01T14:02:03Z\"")?;
        //let query = query_parser.parse_query("* AND NOT date:\"2019-04-01T14:02:03Z\"")?;
        let query = query_parser.parse_query(&query)?;

        let top_docs = searcher.search(&query, &TopDocs::with_limit(100))?;

        for (_score, doc_address) in top_docs {
            let retrieved_doc = searcher.doc(doc_address)?;
            println!("{}", schema.to_json(&retrieved_doc));
        }
    } else {
        // Use interactive fuzzy finder

        let engine_factory = TantivyEngineFactory::builder();
        let options = SkimOptionsBuilder::default()
            .height(Some("50%"))
            .multi(true)
            .preview(Some("")) // preview should be specified to enable preview window
            .engine_factory(Some(Rc::new(engine_factory)))
            .build()
            .unwrap();

        let (tx_item, rx_item): (SkimItemSender, SkimItemReceiver) = unbounded();
        let _ = tx_item.send(Arc::new(MyItem {
            inner: "color aaaa".to_string(),
        }));
        let _ = tx_item.send(Arc::new(MyItem {
            inner: "bbbb".to_string(),
        }));
        let _ = tx_item.send(Arc::new(MyItem {
            inner: "ccc".to_string(),
        }));
        drop(tx_item); // so that skim could know when to stop waiting for more items.

        let selected_items = Skim::run_with(&options, Some(rx_item))
            .map(|out| out.selected_items)
            .unwrap_or_else(|| Vec::new());

        for item in selected_items.iter() {
            print!("{}{}", item.output(), "\n");
        }
    }

    Ok(())
}

struct MyItem {
    inner: String,
}

impl SkimItem for MyItem {
    fn text(&self) -> Cow<str> {
        Cow::Borrowed(&self.inner)
    }

    fn preview(&self, _context: PreviewContext) -> ItemPreview {
        if self.inner.starts_with("color") {
            ItemPreview::AnsiText(format!("\x1b[31mhello:\x1b[m\n{}", self.inner))
        } else {
            ItemPreview::Text(format!("hello:\n{}", self.inner))
        }
    }
}

fn index_file(path: &std::path::PathBuf) -> Result<Doc, io::Error> {
    let s = fs::read_to_string(path.to_str().unwrap())?;

    let (yaml, content) = frontmatter::parse_and_find_content(&s).unwrap();
    match yaml {
        Some(yaml) => {
            let mut out_str = String::new();
            {
                let mut emitter = YamlEmitter::new(&mut out_str);
                emitter.dump(&yaml).unwrap(); // dump the YAML object to a String
            }

            let mut doc: Doc = serde_yaml::from_str(&out_str).unwrap();
            if doc.filename == *"" {
                doc.filename = String::from(path.file_name().unwrap().to_str().unwrap());
            }

            doc.body = content.to_string();

            Ok(doc)
        }
        None => Err(Error::new(
            ErrorKind::Other,
            format!("Failed to process file {}", path.display()),
        )),
    }
}
