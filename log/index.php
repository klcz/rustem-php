<?php
$action = trim(explode('?', $_SERVER['REQUEST_URI'])[0], '/');
rustenphp_error("action: {$action}, args: " . json_encode($_GET), 'info');

if ($action == 'test_json') {
	echo json_encode(['a' => _get('a', 1, 'intval')]);
} else if ($action == 'phpinfo') {
	echo phpinfo();
} else {
	echo "Hello {$_(_get('name', 'world'))}, by rustenphp";
}

