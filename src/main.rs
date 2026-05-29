use std::{env, fs, process, ptr, thread};

use std::ffi::{CString, c_char, c_int};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};

unsafe extern "C" {
    fn php_embed_init(argc: c_int, argv: *mut *mut c_char) -> c_int;
    fn php_embed_shutdown();
    fn zend_eval_string(
        str: *const c_char,
        retval_ptr: *mut std::ffi::c_void,
        string_name: *const c_char,
    ) -> c_int;
}

struct PhpRuntime;

impl PhpRuntime {
    fn boot() -> Result<Self, String> {
        // configure_windows_php_dll_directory()?;

        let arg0 = CString::new("rustenphp-embed").expect("valid arg0");
        let mut argv = [arg0.as_ptr() as *mut c_char];
        let status = unsafe { php_embed_init(1, argv.as_mut_ptr()) };

        if status != 0 {
            return Err(format!("php_embed_init failed with status {status}"));
        }

        Ok(Self)
    }

    fn eval(&self, php: &str) -> Result<(), String> {
        let code =
            CString::new(php).map_err(|_| "PHP code contains an interior null byte".to_string())?;
        let name = CString::new("rustenphp eval").expect("valid eval name");
        let status = unsafe { zend_eval_string(code.as_ptr(), ptr::null_mut(), name.as_ptr()) };

        if status == 0 {
            Ok(())
        } else {
            Err(format!("zend_eval_string failed with status {status}"))
        }
    }
}

impl Drop for PhpRuntime {
    fn drop(&mut self) {
        unsafe {
            php_embed_shutdown();
        }
    }
}

// A payload structure to pass work from the TCP threads to the PHP background thread
struct PhpWorkJob {
    php_code: String,
    output_path: PathBuf,
    response_tx: mpsc::SyncSender<Result<(), String>>,
}

fn main() -> Result<(), String> {
    let args = env::args().skip(1).collect::<Vec<_>>();

    if args.first().is_some_and(|arg| arg == "serve") {
        return serve_laravel();
    }

    let php = args.join(" ");
    let php = if php.trim().is_empty() {
        "echo 'PHP embedded in Rust via php8embed.lib: '.PHP_VERSION.PHP_EOL;".to_string()
    } else {
        php
    };

    let runtime = PhpRuntime::boot()?;
    runtime.eval(&php)?;

    Ok(())
}

fn serve_laravel() -> Result<(), String> {
    let laravel_root = env::var("RUSTENPHP_LARAVEL_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|_| env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let public_root = laravel_root.join("logs");

    // Change directory to Laravel root ONCE at startup
    env::set_current_dir(&laravel_root).map_err(|error| {
        format!(
            "failed to chdir to Laravel root {}: {error}",
            laravel_root.display()
        )
    })?;

    // Create a channel for assigning work tasks to the background PHP worker
    let (tx, rx) = mpsc::channel::<PhpWorkJob>();

    // Spawn ONE dedicated background thread where the PHP Engine stays booted forever
    thread::spawn(move || {
        println!("Booting persistent PHP runtime background worker thread...");

        // Listen continuously for incoming execution strings from TCP workers
        for job in rx {
            let runtime =
                PhpRuntime::boot().expect("Failed to boot persistent background PHP runtime");

            let _ = &job.output_path;
            let eval_result = runtime.eval(&job.php_code);
            // Inform the requesting TCP connection thread that compilation completed
            let _ = job.response_tx.send(eval_result);
        }
    });

    let host = env::var("RUSTENPHP_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port = env::var("RUSTENPHP_PORT").unwrap_or_else(|_| "8787".to_string());
    let address = format!("{host}:{port}");
    let listener = TcpListener::bind(&address)
        .map_err(|error| format!("failed to bind {address}: {error}"))?;
    let schema = "http";

    println!("rustenphp embedded Laravel server listening on {schema}://{address}");

    for stream in listener.incoming() {
        match stream {
            Ok(mut stream) => {
                let tx_clone = tx.clone();
                let laravel_root_clone = laravel_root.clone();
                let public_root_clone = public_root.clone();
                let host_clone = host.clone();
                let port_clone = port.clone();

                // Handle network operations asynchronously using native multi-threading
                thread::spawn(move || {
                    if let Err(error) = handle_connection(
                        &mut stream,
                        &laravel_root_clone,
                        &public_root_clone,
                        &host_clone,
                        &port_clone,
                        &tx_clone,
                    ) {
                        let body = format!("rustenphp error: {error}");
                        let _ = write_response(
                            &mut stream,
                            500,
                            "text/plain; charset=utf-8",
                            body.as_bytes(),
                        );
                    }
                });
            }
            Err(error) => eprintln!("connection failed: {error}"),
        }
    }

    Ok(())
}

fn handle_connection(
    stream: &mut TcpStream,
    _laravel_root: &Path,
    public_root: &Path,
    host: &str,
    port: &str,
    php_worker_tx: &Sender<PhpWorkJob>,
) -> Result<(), String> {
    let request = read_http_request(stream)?;
    let Some(request_line) = request.lines().next() else {
        return Err("empty HTTP request".to_string());
    };

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("GET");
    let target = parts.next().unwrap_or("/");
    let (path, query) = split_target(target);

    if method != "GET" && method != "HEAD" {
        return write_response(
            stream,
            405,
            "text/plain; charset=utf-8",
            b"Only GET and HEAD are implemented in this prototype.",
        );
    }

    // Find this block inside your handle_connection function
    if let Some(static_file) = resolve_static_file(public_root, path) {
        let body = fs::read(&static_file).map_err(|error| {
            format!(
                "failed to read static file {}: {error}",
                static_file.display()
            )
        })?;
        return write_response(stream, 200, content_type(&static_file), &body);
    }

    let request_id = rustenphp_request_id("req");
    // UPDATE THIS LINE to pass laravel_root forward:
    let body = run_laravel_request(
        _laravel_root,
        public_root,
        &request_id,
        method,
        target,
        query,
        host,
        port,
        php_worker_tx,
    )?;
    let mut content_type = "text/html; charset=utf-8";
    if (body.trim().starts_with("{") && body.trim().ends_with("}"))
        || (body.trim().starts_with("[") && body.trim().ends_with("]"))
    {
        content_type = "application/json; charset=utf-8";
    }
    write_response(stream, 200, content_type, body.as_bytes())
}

fn read_http_request(stream: &mut TcpStream) -> Result<String, String> {
    let mut buffer = [0_u8; 8192];
    let bytes_read = stream
        .read(&mut buffer)
        .map_err(|error| format!("failed to read request: {error}"))?;

    Ok(String::from_utf8_lossy(&buffer[..bytes_read]).to_string())
}

fn split_target(target: &str) -> (&str, &str) {
    target
        .split_once('?')
        .map_or((target, ""), |(path, query)| (path, query))
}

fn resolve_static_file(public_root: &Path, path: &str) -> Option<PathBuf> {
    let path = path.trim_start_matches('/').replace('\\', "/");

    if path.is_empty() || path.contains("..") {
        return None;
    }

    let candidate = public_root.join(path);

    if candidate.is_file() {
        let ext = candidate
            .extension()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if ext != "php" && ext != "log" {
            return Some(candidate);
        }
    }

    None
}

fn run_laravel_request(
    laravel_root: &Path, // <-- Add this line back in!
    public_root: &Path,
    request_id: &str,
    method: &str,
    target: &str,
    query: &str,
    host: &str,
    port: &str,
    php_worker_tx: &Sender<PhpWorkJob>,
) -> Result<String, String> {
    let output_path = public_root.join("tmp").join(&request_id);

    let index_path = public_root.join("index.php");
    let log_path = public_root.join("rustenphp-error.log");
    let php = format!(
        r#"
$__rustenphp_output_path = {output_path};
error_reporting(E_ERROR);
ini_set('display_errors', '1');
date_default_timezone_set('Asia/Shanghai');

function rustenphp_error($v, $action = 'error', $_level = 0) {{
    $__rustenphp_log_path = {log_path};
    if ($action === 'warn' || $action === 'error' || $action === 'fault' ||
            $action === 'debug' || $action === 'info' || $action === 'log')  {{
        $now = microtime(true);
        $action = $action === 'log' ? 'INFO' : strtoupper($action);
        $_M = ['DEBUG' => 10, 'INFO' => 20, 'WARN' => 30, 'ERROR' => 40, 'FAULT' => 50];
        if (!empty($_M[$action]) && $_M[$action] >= $_level) {{
            $_ms = intval(($now - intval($now)) * 1000);
            $_ms = $_ms >= 100 ? $_ms : ($_ms >= 10 ? "0$_ms" : "00$_ms");
            $_msg = date('[Y-m-d H:i:s', $now) . ".$_ms] `$action` $v\n";
            error_log($_msg, 3, $__rustenphp_log_path);
        }}
    }}
}};

function _get($n, $d, $t='') {{
    $v = isset($_GET[$n]) ? $_GET[$n] : $d;
    $v = $t == 'intval' ? intval($v) : $v;
    $v = $t == 'floatval' ? floatval($v) : $v;
    return $v;
}}

/** @noinspection PhpUnusedLocalVariableInspection */
$_t = microtime(true);
/** @noinspection PhpUnusedLocalVariableInspection */
$_ = function ($v, $action = '') {{
    global $_t;
    if (empty($action)) {{
        return $v;
    }}
    if ($action === 'echo') {{
        echo $v;
    }} elseif ($action === 'done') {{
        $now = microtime(true);
        $_ms = intval(($now - $_t) * 1000.0 * 100.0) / 100.0;
        $_ms = $_ms < 1000.0 ? "{{$_ms}} ms" : (intval($_ms / 1000.0 * 100.0) / 100.0) . " s";
        rustenphp_error("\n\n====================================\nDone {{$v}} Use: {{$_ms}}.\n====================================\n", 'info');
        $_t = $now;
        return $_ms;
    }} elseif ($action === 'log' || $action === 'debug' || $action === 'info' ||
        $action === 'warn' || $action === 'error' || $action === 'fault') {{
        rustenphp_error($v, $action);
    }}
    return $v;
}};

register_shutdown_function(function () use ($__rustenphp_output_path) {{
    $error = error_get_last();
    if ($error && !file_exists($__rustenphp_output_path)) {{
        $_msg = "PHP fatal error: ".$error['message']." in ".$error['file'].":".$error['line'];
        file_put_contents($__rustenphp_output_path, $_msg);
        rustenphp_error($_msg);
    }}
}});

try {{
    // 1. Setup the standard request superglobals for this specific turn
    $_SERVER['REQUEST_METHOD'] = {method};
    $_SERVER['REQUEST_URI'] = {target};
    $_SERVER['REQUEST_ID'] = '{request_id}';
    $_SERVER['QUERY_STRING'] = {query};
    $_SERVER['SCRIPT_FILENAME'] = {script_filename};
    $_SERVER['SCRIPT_NAME'] = '/index.php';
    $_SERVER['PHP_SELF'] = '/index.php';
    $_SERVER['DOCUMENT_ROOT'] = {document_root};
    $_SERVER['SERVER_NAME'] = {host};
    $_SERVER['SERVER_PORT'] = {port};
    $_SERVER['SERVER_PROTOCOL'] = 'HTTP/1.1';
    $_SERVER['HTTP_HOST'] = {http_host};
    $_SERVER['HTTPS'] = 'off';
    
    $_GET = [];
    parse_str({query}, $_GET);
    $_POST = [];
    $_REQUEST = $_GET;

    // 2. Clear out any previous output buffers left over
    while (ob_get_level() > 0) {{
        ob_end_clean();
    }}
    ob_start();

    // 3. Define LARAVEL_START conditionally if it doesn't exist in our global runtime yet
    if (!defined('LARAVEL_START')) {{
        define('LARAVEL_START', microtime(true));
    }}

    // 4. Use require_once for Composer's autoloader so it doesn't re-register functions
    // require_once {laravel_root} . '/vendor/autoload.php';
    require_once {document_root} . '/index.php';

    // 5. Boot or resolve the Kernel application instance
    // We check if we already booted the app in a previous request to keep it ultra-fast

    /* global $__rustenphp_app, $__rustenphp_kernel;
    if (!$__rustenphp_app) {{
        $__rustenphp_app = require_once {laravel_root} . '/bootstrap/app.php';
        $__rustenphp_kernel = $__rustenphp_app->make(Illuminate\Contracts\Http\Kernel::class);
    }}

    // 6. Handle the incoming request through the persistent kernel
    $__rustenphp_request = Illuminate\Http\Request::capture();
    $__rustenphp_response = $__rustenphp_kernel->handle($__rustenphp_request);
    $__rustenphp_response->send();
    $__rustenphp_kernel->terminate($__rustenphp_request, $__rustenphp_response); */

    $__rustenphp_body = ob_get_clean();
    file_put_contents($__rustenphp_output_path, $__rustenphp_body);

}} catch (Throwable $exception) {{
    while (ob_get_level() > 0) {{
        ob_end_clean();
    }}
    $_msg = "PHP throwable: ".$exception::class.": ".$exception->getMessage()." in ".$exception->getFile().":".$exception->getLine();
    file_put_contents($__rustenphp_output_path, $_msg);
    rustenphp_error($_msg);
}}
"#,
        method = php_string(method),
        target = php_string(target),
        query = php_string(query),
        host = php_string(host),
        port = php_string(port),
        request_id = request_id,
        http_host = php_string(&format!("{host}:{port}")),
        script_filename = php_string(&index_path.to_string_lossy()),
        document_root = php_string(&public_root.to_string_lossy()),
        laravel_root = php_string(&laravel_root.to_string_lossy()),
        output_path = php_string(&output_path.to_string_lossy()),
        log_path = php_string(&log_path.to_string_lossy()),
    );
    // Create a local synchronous single-use response back-channel
    let (reply_tx, reply_rx) = mpsc::sync_channel(0);

    let job = PhpWorkJob {
        php_code: php,
        output_path: output_path.clone(),
        response_tx: reply_tx,
    };

    // Send the execution job down to the background PHP thread
    php_worker_tx
        .send(job)
        .map_err(|_| "PHP background worker thread panicked or stopped".to_string())?;

    // Block the local network connection thread until the background PHP engine responds
    reply_rx
        .recv()
        .map_err(|_| "Failed to receive response from PHP worker".to_string())??;

    let body = fs::read_to_string(&output_path).map_err(|error| {
        format!(
            "failed to read PHP response {}: {error}",
            output_path.display()
        )
    })?;
    let _ = fs::remove_file(&output_path);
    let _ = fs::remove_file(format!("{:?}{}", output_path, ".post"));
    Ok(body)
}

fn write_response(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> Result<(), String> {
    let reason = match status {
        200 => "OK",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        _ => "OK",
    };
    let headers = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );

    stream
        .write_all(headers.as_bytes())
        .and_then(|_| stream.write_all(body))
        .map_err(|error| format!("failed to write response: {error}"))
}

fn content_type(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
    {
        "css" => "text/css; charset=utf-8",
        "js" => "application/javascript; charset=utf-8",
        "json" => "application/json; charset=utf-8",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "txt" => "text/html; charset=utf-8",
        "log" => "text/html; charset=utf-8",
        _ => "application/octet-stream",
    }
}

fn php_string(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('\'', "\\'")
        .replace('\r', "\\r")
        .replace('\n', "\\n");

    format!("'{escaped}'")
}

static RUSTENPHP_REQUEST_COUNTER: AtomicU64 = AtomicU64::new(0);
fn rustenphp_request_id(prefix: &str) -> String {
    let seq = RUSTENPHP_REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed);

    let now = std::time::SystemTime::now();
    let timestamp = now
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let node_id = process::id() as u64;

    let time_low = (timestamp & 0xFFFFFFFF) as u32;
    let time_mid = ((timestamp >> 32) & 0xFFFF) as u16;
    let clock_seq = (seq & 0x3FFF) as u16 | 0x8000;
    let node = (node_id & 0xFFFF) as u16;

    // UUID 字符串: 8-4-4-4
    format!(
        "{}-{:08x}-{:04x}-{:04x}-{:04x}",
        prefix, time_low, clock_seq, time_mid, node,
    )
}
