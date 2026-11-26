<?php

declare(strict_types=1);

namespace Hashinbox;

use InvalidArgumentException;

/**
 * Hand-written: a scope's text that does not parse.
 */
final class ScopeError extends InvalidArgumentException {}
