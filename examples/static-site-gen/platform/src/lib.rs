use core::ffi::c_void;
use libc;
use pulldown_cmark::{html, Options, Parser};
use roc_std::RocStr;
use std::env;
use std::ffi::CStr;
use std::fs;
use std::os::raw::c_char;
use std::path::{Path, PathBuf};

extern "C" {
    #[link_name = "roc__transformFileContentForHost_1_exposed"]
    fn roc_transformFileContentForHost(relPath: &RocStr, content: &RocStr) -> RocStr;
}

#[no_mangle]
pub unsafe extern "C" fn roc_alloc(size: usize, _alignment: u32) -> *mut c_void {
    libc::malloc(size)
}

#[no_mangle]
pub unsafe extern "C" fn roc_realloc(
    c_ptr: *mut c_void,
    new_size: usize,
    _old_size: usize,
    _alignment: u32,
) -> *mut c_void {
    libc::realloc(c_ptr, new_size)
}

#[no_mangle]
pub unsafe extern "C" fn roc_dealloc(c_ptr: *mut c_void, _alignment: u32) {
    libc::free(c_ptr)
}

#[cfg(unix)]
#[no_mangle]
pub unsafe extern "C" fn roc_getppid() -> libc::pid_t {
    libc::getppid()
}

#[cfg(unix)]
#[no_mangle]
pub unsafe extern "C" fn roc_mmap(
    addr: *mut libc::c_void,
    len: libc::size_t,
    prot: libc::c_int,
    flags: libc::c_int,
    fd: libc::c_int,
    offset: libc::off_t,
) -> *mut libc::c_void {
    libc::mmap(addr, len, prot, flags, fd, offset)
}

#[cfg(unix)]
#[no_mangle]
pub unsafe extern "C" fn roc_shm_open(
    name: *const libc::c_char,
    oflag: libc::c_int,
    mode: libc::mode_t,
) -> libc::c_int {
    libc::shm_open(name, oflag, mode as libc::c_uint)
}

#[no_mangle]
pub extern "C" fn rust_main() -> i32 {
    let args: Vec<String> = env::args().collect();
    if args.len() != 3 {
        eprintln!("Usage: {} path/to/input/dir path/to/output/dir", args[0]);
        return 1;
    }

    match run(&args[1], &args[2]) {
        Err(e) => {
            eprintln!("{}", e);
            1
        }
        Ok(()) => 0,
    }
}

#[no_mangle]
pub unsafe extern "C" fn roc_panic(c_ptr: *mut c_void, tag_id: u32) {
    match tag_id {
        0 => {
            let slice = CStr::from_ptr(c_ptr as *const c_char);
            let string = slice.to_str().unwrap();
            eprintln!("Roc hit a panic: {}", string);
            std::process::exit(1);
        }
        _ => todo!(),
    }
}

#[no_mangle]
pub unsafe extern "C" fn roc_memcpy(
    dest: *mut c_void,
    src: *const c_void,
    bytes: usize,
) -> *mut c_void {
    libc::memcpy(dest, src, bytes)
}

#[no_mangle]
pub unsafe extern "C" fn roc_memset(dst: *mut c_void, c: i32, n: usize) -> *mut c_void {
    libc::memset(dst, c, n)
}

fn run(input_dirname: &str, output_dirname: &str) -> Result<(), String> {
    let input_dir = strip_windows_prefix(
        PathBuf::from(input_dirname)
            .canonicalize()
            .map_err(|e| format!("{}: {}", input_dirname, e))?,
    );

    let output_dir = {
        let dir = PathBuf::from(output_dirname);
        if !dir.exists() {
            fs::create_dir_all(&dir).unwrap();
        }
        strip_windows_prefix(
            dir.canonicalize()
                .map_err(|e| format!("{}: {}", output_dirname, e))?,
        )
    };

    if !input_dir.exists() {
        return Err(format!("{} does not exist. The first argument should be the directory where your Markdown files are.", input_dir.display()));
    }

    if !output_dir.exists() {
        return Err(format!("Could not create {}", output_dir.display()));
    }

    let mut input_files: Vec<PathBuf> = vec![];
    find_files(&input_dir, &mut input_files)
        .map_err(|e| format!("Error finding input files: {}", e))?;

    println!("Processing {} input files...", input_files.len());

    // TODO: process the files asynchronously
    let num_files = input_files.len();
    let mut num_errors = 0;
    let mut num_successes = 0;
    for input_file in input_files {
        match input_file.extension() {
            Some(s) if s.eq("md".into()) => {
                match process_file(&input_dir, &output_dir, &input_file) {
                    Ok(()) => {
                        num_successes += 1;
                    }
                    Err(e) => {
                        eprintln!(
                            "Failed to process file:\n\n  ({:?})with error:\n\n  {}",
                            &input_file, e
                        );
                        num_errors += 1;
                    }
                }
            }
            _ => {}
        };
    }

    println!(
        "Processed {} files with {} successes and {} errors",
        num_files, num_successes, num_errors
    );

    if num_errors > 0 {
        Err("Could not process all files".into())
    } else {
        Ok(())
    }
}

fn process_file(input_dir: &Path, output_dir: &Path, input_file: &Path) -> Result<(), String> {
    let input_relpath = input_file
        .strip_prefix(input_dir)
        .map_err(|e| e.to_string())?
        .to_path_buf();

    let mut output_relpath = input_relpath.clone();
    output_relpath.set_extension("html");

    let content_md = fs::read_to_string(input_file).map_err(|e| {
        format!(
            "Error reading {}: {}",
            input_file.to_str().unwrap_or("an input file"),
            e
        )
    })?;

    let mut content_html = String::new();
    let mut options = Options::all();

    // In the tutorial, this messes up string literals in <samp> blocks.
    // Those could be done as markdown code blocks, but the repl ones need
    // a special class, and there's no way to add that class using markdown alone.
    //
    // We could make this option user-configurable if people actually want it!
    options.remove(Options::ENABLE_SMART_PUNCTUATION);

    let parser = Parser::new_ext(&content_md, options);

    // We'll build a new vector of events since we can only consume the parser once
    let mut parser_with_highlighting = Vec::new();
    // As we go along, we'll want to highlight code in bundles, not lines
    let mut to_highlight = String::new();
    // And track a little bit of state
    let mut in_code_block = false;
    let mut is_roc_code = false;

    for event in parser {
        match event {
            pulldown_cmark::Event::Code(cow_str) => {
                let highlighted_html =
                    roc_highlight::highlight_roc_code_inline(cow_str.to_string().as_str());
                parser_with_highlighting.push(pulldown_cmark::Event::Html(
                    pulldown_cmark::CowStr::from(highlighted_html),
                ));
            }
            pulldown_cmark::Event::Start(pulldown_cmark::Tag::CodeBlock(cbk)) => {
                in_code_block = true;
                is_roc_code = is_roc_code_block(&cbk);
            }
            pulldown_cmark::Event::End(pulldown_cmark::Tag::CodeBlock(_)) => {
                if in_code_block {
                    // Format the whole multi-line code block as HTML all at once
                    let highlighted_html: String;
                    if is_roc_code {
                        highlighted_html = roc_highlight::highlight_roc_code(&to_highlight)
                    } else {
                        highlighted_html = format!("<pre><samp>{}</pre></samp>", &to_highlight)
                    }

                    // And put it into the vector
                    parser_with_highlighting.push(pulldown_cmark::Event::Html(
                        pulldown_cmark::CowStr::from(highlighted_html),
                    ));
                    to_highlight = String::new();
                    in_code_block = false;
                }
            }
            pulldown_cmark::Event::Text(t) => {
                if in_code_block {
                    // If we're in a code block, build up the string of text
                    to_highlight.push_str(&t);
                } else {
                    parser_with_highlighting.push(pulldown_cmark::Event::Text(t))
                }
            }
            e => {
                parser_with_highlighting.push(e);
            }
        }
    }

    html::push_html(&mut content_html, parser_with_highlighting.into_iter());

    let roc_relpath = RocStr::from(output_relpath.to_str().unwrap());
    let roc_content_html = RocStr::from(content_html.as_str());
    let roc_output_str =
        unsafe { roc_transformFileContentForHost(&roc_relpath, &roc_content_html) };

    let output_file = output_dir.join(&output_relpath);
    let rust_output_str: &str = &roc_output_str;

    println!("{} -> {}", input_file.display(), output_file.display());

    // Create parent directory if it doesn't exist
    let parent_dir = output_file.parent().unwrap();
    if !parent_dir.exists() {
        fs::create_dir_all(&parent_dir).unwrap();
    }

    fs::write(output_file, rust_output_str).map_err(|e| format!("{}", e))
}

fn find_files(dir: &Path, file_paths: &mut Vec<PathBuf>) -> std::io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let pathbuf = entry?.path();
        if pathbuf.is_dir() {
            find_files(&pathbuf, file_paths)?;
        } else {
            file_paths.push(pathbuf);
        }
    }
    Ok(())
}

/// On windows, the path is prefixed with `\\?\`, the "verbatim" prefix.
/// Such a path does not works as an argument to `zig` and other command line tools,
/// and there seems to be no good way to strip it. So we resort to some string manipulation.
pub fn strip_windows_prefix(path_buf: PathBuf) -> std::path::PathBuf {
    #[cfg(not(windows))]
    return path_buf;

    let path_str = path_buf.display().to_string();

    std::path::Path::new(path_str.trim_start_matches(r"\\?\")).to_path_buf()
}

fn is_roc_code_block(cbk: &pulldown_cmark::CodeBlockKind) -> bool {
    match cbk {
        pulldown_cmark::CodeBlockKind::Indented => false,
        pulldown_cmark::CodeBlockKind::Fenced(cow_str) => {
            if cow_str.contains("roc") {
                true
            } else {
                false
            }
        }
    }
}
