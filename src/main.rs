use anyhow::Result;
use chrono::{DateTime, FixedOffset};
use clap::{App, Arg, ArgMatches, SubCommand};
use glob::{glob, Paths};
use serde::{de, Deserialize, Deserializer, Serialize};
use std::convert::From;
use std::io::{Error, ErrorKind};
use std::str;
use std::{ffi::OsString, fmt, fs, io, io::Read, marker::PhantomData, path::Path};
use tantivy::{collector::TopDocs, doc, query::QueryParser, schema::*, Index};
use toml::Value as tomlVal;
use yaml_rust::YamlEmitter;

mod util;

use crate::util::event::{Event, Events};
use termion::{event::Key, input::MouseTerminal, raw::IntoRawMode, screen::AlternateScreen};
use tui::{
    backend::TermionBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Span, Spans},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Terminal,
};
use unicode_width::UnicodeWidthStr;

/// Example FrontMatter + Markdown doc to index:
///
/// ---
/// author: Steve Sosik
/// date: 2021-06-22T12:48:16-0400
/// tags:
/// - tika
/// title: This is an example note
/// ---
///
/// Some note here formatted with Markdown syntax
///

/// TerminalApp holds the state of the application
struct TerminalApp {
    /// Current value of the input box
    input: String,
    /// Query Matches
    matches: Vec<String>,
    /// Keep track of which matches are selected
    state: ListState,
}

impl TerminalApp {
    pub fn get_selected(&mut self) -> Vec<String> {
        let mut ret: Vec<String> = Vec::new();
        if let Some(i) = self.state.selected() {
            ret.push(self.matches[i].clone());
        };
        ret
    }

    pub fn next(&mut self) {
        let i = match self.state.selected() {
            Some(i) => {
                if i >= self.matches.len() - 1 {
                    0
                } else {
                    i + 1
                }
            }
            None => 0,
        };
        self.state.select(Some(i));
    }

    pub fn previous(&mut self) {
        let i = match self.state.selected() {
            Some(i) => {
                if i == 0 {
                    self.matches.len() - 1
                } else {
                    i - 1
                }
            }
            None => 0,
        };
        self.state.select(Some(i));
    }
}

impl Default for TerminalApp {
    fn default() -> TerminalApp {
        TerminalApp {
            input: String::new(),
            matches: Vec::new(),
            state: ListState::default(),
        }
    }
}

/// Representation for a given Markdown + FrontMatter file
#[derive(Debug, PartialEq, Serialize, Deserialize)]
struct TikaDocument {
    /// Inherent metadata about the document
    #[serde(default)]
    filename: String,
    #[serde(skip_deserializing)]
    full_path: OsString,

    /// FrontMatter-derived metadata about the document
    #[serde(default)]
    author: String,
    date: String,
    /// RFC 3339 based timestamp
    #[serde(deserialize_with = "string_or_list_string")]
    tags: Vec<String>,
    title: String,

    /// The Markdown-formatted body of the document
    #[serde(skip_deserializing)]
    body: String,
}

/// Support Deserializing a string into a list of string of length 1
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

impl From<TantivyDoc> for TikaDocument {
    fn from(item: TantivyDoc) -> Self {
        TikaDocument {
            filename: item
                .retrieved_doc
                .get_first(item.filename)
                .unwrap()
                .text()
                .unwrap_or("")
                .into(),
            author: item
                .retrieved_doc
                .get_first(item.author)
                .unwrap()
                .text()
                .unwrap_or("")
                .into(),
            title: item
                .retrieved_doc
                .get_first(item.title)
                .unwrap()
                .text()
                .unwrap_or("")
                .into(),
            body: String::from(""),
            date: item
                .retrieved_doc
                .get_first(item.date)
                .unwrap()
                .text()
                .unwrap_or("")
                .into(),
            tags: vec![String::from("foo")],
            full_path: OsString::from(""),
        }
    }
}

fn main() -> Result<()> {
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
            Arg::with_name("v")
                .short("v")
                .multiple(true)
                .help("Sets the level of verbosity"),
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

    let filename = schema_builder.add_text_field("filename", TEXT | STORED);
    let full_path = schema_builder.add_text_field("full_path", TEXT | STORED);

    let author = schema_builder.add_text_field("author", TEXT | STORED);
    let date = schema_builder.add_date_field("date", INDEXED | STORED);
    let tags = schema_builder.add_text_field("tags", TEXT | STORED);
    let title = schema_builder.add_text_field("title", TEXT | STORED);

    let body = schema_builder.add_text_field("body", TEXT);

    let schema = schema_builder.build();

    let index = Index::create_in_ram(schema.clone());
    let mut index_writer = index.writer(100_000_000).unwrap();

    for entry in glob_files(&cli).expect("Failed to read glob pattern") {
        match entry {
            Ok(path) => {
                if let Ok(doc) = index_file(&path) {
                    let t: DateTime<FixedOffset>;
                    if let Ok(rfc3339) = DateTime::parse_from_rfc3339(&doc.date) {
                        t = rfc3339;
                    } else if let Ok(s) =
                        DateTime::parse_from_str(&doc.date, &String::from("%Y-%m-%dT%T%z"))
                    {
                        t = s;
                    } else {
                        eprintln!("❌ Failed to convert path to str '{}'", path.display());
                        continue;
                    }
                    if let Some(f) = path.to_str() {
                        index_writer.add_document(doc!(
                            author => doc.author,
                            body => doc.body,
                            date => Value::Date(t.with_timezone(&chrono::Utc)),
                            filename => doc.filename,
                            full_path => f,
                            tags => doc.tags.join(" "),
                            title => doc.title,
                        ));
                        if cli.occurrences_of("v") > 0 {
                            println!("✅ {}", f);
                        }
                    } else {
                        eprintln!(
                            "❌ Failed to parse time '{}' from {}",
                            doc.date, doc.filename
                        );
                    }
                } else {
                    eprintln!("❌ Failed to load file {}", path.display());
                }
            }

            Err(e) => eprintln!("❌ {:?}", e),
        }
    }

    index_writer.commit().unwrap();

    let reader = index.reader()?;
    let searcher = reader.searcher();
    let query_parser = QueryParser::for_index(&index, vec![author, body, filename, tags, title]);

    let mut selected: Vec<String> = Vec::new();

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
            let td: TikaDocument = TantivyDoc {
                retrieved_doc,
                author,
                date,
                filename,
                //full_path,
                //tags,
                title,
            }
            .into();
            let it = serde_json::to_string(&td).unwrap();
            println!("{}", it);
        }
    } else {
        // Use interactive fuzzy finder

        // Terminal initialization
        let stdout = io::stdout().into_raw_mode()?;
        let stdout = MouseTerminal::from(stdout);
        let stdout = AlternateScreen::from(stdout);
        let backend = TermionBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;

        // Setup event handlers
        let events = Events::new();

        // Create default app state
        let mut app = TerminalApp::default();

        loop {
            // Draw UI
            terminal.draw(|f| {
                let chunks = Layout::default()
                    .direction(Direction::Vertical)
                    .margin(2)
                    .constraints([Constraint::Min(1), Constraint::Length(3)].as_ref())
                    .split(f.size());
                let selected_style = Style::default().add_modifier(Modifier::REVERSED);

                // Output area where match titles are displayed
                let matches: Vec<ListItem> = app
                    .matches
                    .iter()
                    .enumerate()
                    .map(|(i, m)| {
                        let content = vec![Spans::from(Span::raw(format!("{}: {}", i, m)))];
                        ListItem::new(content)
                    })
                    .collect();
                let matches = List::new(matches)
                    .block(Block::default().borders(Borders::ALL))
                    .highlight_style(selected_style);
                //.highlight_symbol("> ");
                f.render_stateful_widget(matches, chunks[0], &mut app.state);

                // Input area where queries are entered
                let input = Paragraph::new(app.input.as_ref())
                    .style(Style::default().fg(Color::Yellow))
                    .block(Block::default().borders(Borders::ALL));
                f.render_widget(input, chunks[1]);
                // Make the cursor visible and ask tui-rs to put it at the specified coordinates after rendering
                f.set_cursor(
                    // Put cursor past the end of the input text
                    chunks[1].x + app.input.width() as u16 + 1,
                    // Move one line down, from the border to the input line
                    chunks[1].y + 1,
                )
            })?;

            // Handle input
            if let Event::Input(input) = events.next()? {
                match input {
                    Key::Char('\n') => {
                        selected = app.get_selected();
                        println!("DONE");
                        break;
                    }
                    Key::Ctrl('c') => {
                        break;
                    }
                    Key::Char(c) => {
                        app.input.push(c);
                    }
                    Key::Backspace => {
                        app.input.pop();
                    }
                    Key::Down => {
                        app.next();
                    }
                    Key::Up => {
                        app.previous();
                    }
                    _ => {}
                }

                let query = query_parser.parse_query(&app.input)?;
                let top_docs = searcher.search(&query, &TopDocs::with_limit(100))?;

                app.matches = Vec::new();
                for (_score, doc_address) in top_docs {
                    let retrieved_doc = searcher.doc(doc_address)?;
                    app.matches.push(
                        retrieved_doc
                            .get_first(title)
                            .unwrap()
                            .text()
                            .unwrap()
                            .into(),
                    );
                }
            }
        }
    }

    for sel in selected {
        println!("{}", sel);
    }
    Ok(())
}

fn glob_files(cli: &ArgMatches) -> Result<Paths, Box<dyn std::error::Error>> {
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

    if cli.occurrences_of("v") > 0 {
        println!("Sourcing Markdown documents matching : {}", glob_str);
    }

    return Ok(glob(&glob_str)?);
}

struct TantivyDoc {
    retrieved_doc: Document,
    author: Field,
    date: Field,
    filename: Field,
    //full_path: Field,
    //tags: Field,
    title: Field,
}

fn index_file(path: &std::path::PathBuf) -> Result<TikaDocument, io::Error> {
    let s = fs::read_to_string(path.to_str().unwrap())?;

    let (yaml, content) = frontmatter::parse_and_find_content(&s).unwrap();
    match yaml {
        Some(yaml) => {
            let mut out_str = String::new();
            {
                let mut emitter = YamlEmitter::new(&mut out_str);
                emitter.dump(&yaml).unwrap(); // dump the YAML object to a String
            }

            let mut doc: TikaDocument = serde_yaml::from_str(&out_str).unwrap();
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
