<?php

declare(strict_types=1);

namespace Hashinbox;

use RuntimeException;

/**
 * Hand-written: a call the session cannot make, refused here before it is sent.
 */
final class MissingScope extends RuntimeException
{
    public function __construct(public readonly string $operation, public readonly string $scope)
    {
        parent::__construct("{$operation} needs scope {$scope}");
    }
}
