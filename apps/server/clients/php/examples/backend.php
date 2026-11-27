<?php

/**
 * A backend holding a key: it creates a wallet for a customer, then issues
 * that customer's browser a 15-minute token reading only that wallet. The
 * secret stays here; the client signs its own sessions with it.
 *
 *     HASHINBOX_URL=… HASHINBOX_KEY_ID=hip/… HASHINBOX_KEY_SECRET=his/… php examples/backend.php
 */

declare(strict_types=1);

namespace Hashinbox\Examples;

use Hashinbox\Client;
use Hashinbox\CreateWalletRequest;
use Hashinbox\Scope;
use Hashinbox\Scopes;
use Hashinbox\Secret;
use Hashinbox\WalletsCreateRequest;

use function Hashinbox\walletsCreate;

require __DIR__ . '/../vendor/autoload.php';

$secret = Secret::parse((string) getenv('HASHINBOX_KEY_ID'), (string) getenv('HASHINBOX_KEY_SECRET'));
$api = new Client((string) getenv('HASHINBOX_URL'), secret: $secret);
$body = new CreateWalletRequest(chain: 'ethereum', network: 'mainnet', externalId: 'customer-1');
$wallet = walletsCreate($api, new WalletsCreateRequest($body));
$scope = Scope::fill('com.hashinbox.wallets[{id}]:read', ['id' => $wallet->id]);
$token = $secret->issue(Scopes::parse($scope), subject: 'customer-1', ttl: 15 * 60);
echo $wallet->id, "\n", $token->token, "\n";
