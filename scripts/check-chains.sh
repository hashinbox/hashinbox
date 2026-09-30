#!/bin/sh
# Chain-crate rules the linter cannot express (a retired design note (002), 004, 007, 008, 011).
# native/ is protocol code only: no SDK contracts, no adapter modules.
# Run from the repository root. Exit 1 lists every violation.
set -u

# Check 4 (every scenario demonstrates every chain, a retired design note (004)) is on by
# default; CHECK_EXAMPLES=0 skips it while a new chain is being scaffolded.
CHECK_EXAMPLES=${CHECK_EXAMPLES:-1}

w='(^|[^A-Za-z0-9_])'
traits='NativeBlock|BlockSource|BlockInterpreter|WalletFactory|AddressParser|FactoryProfile|Wallet|Author|Authorship|Sender|SingleSender|TransactionFactory|Addresser|Signer|TransactionBuilder|Broadcaster'
status=0
dirs=$(ls -d sdk/chains/*/src/native 2>/dev/null)
if [ -n "$dirs" ]; then
  # 1. SDK contract crates in any form: indexing::x, wallets::{..}, `extern crate wallets;`.
  grep -rnE --include='*.rs' "${w}(indexing|wallets)[[:space:]]*(::|\{|;|[[:space:]]as[[:space:]])" $dirs && status=1
  # 2. Adapter modules: any crate:: path that leaves crate::native.
  grep -rnE --include='*.rs' "${w}crate::" $dirs | grep -vE "crate::native([^A-Za-z0-9_]|$)" && status=1
  # 3. SDK trait impls belong in the adapter files (base::Signer as an *input* is fine).
  #    Test doubles below a file's first #[cfg(test)] stand in for such inputs and are exempt.
  impl_re="^[[:space:]]*impl.*${w}(${traits})(<[^>]*>)?[[:space:]]+for[[:space:]]"
  for file in $(grep -rlE --include='*.rs' "$impl_re" $dirs); do
    awk -v re="$impl_re" '/^[[:space:]]*#\[cfg\(test\)\]/ { exit } $0 ~ re { print FILENAME ":" FNR ":" $0; found = 1 }
      END { exit !found }' "$file" && status=1
  done
fi
# 5. native/ takes only primitive inputs from base (docs/CHAIN_LAYOUT.md section 2):
#    block coordinates, amounts, signing inputs, envelopes, chain metadata, and the
#    block/transaction vocabulary. SDK identities (Network, Address,
#    TransactionId, TransactionRef) and SDK errors (TransactionError) are
#    adapter types: conversions to them live in identity.rs and error.rs.
#    crypto is a primitive package and stays unrestricted in native/, except
#    for the one name that carries a placement rule: KeyPair is the local
#    signing engine of every chain — secp256k1 for Bitcoin and Ethereum,
#    Ed25519 for Solana — so crypto::KeyPair (base::KeyPair before it moved
#    down) belongs to native/key.rs and tests only.
base_items='BlockRef|BlockHash|BlockHeight|BlockPosition|BlockParent|Decimal|DecimalError|BoxFuture'
base_items="$base_items|Signer|SignRequest|SignFuture|SignedPayload|Signature|SignatureScheme"
base_items="$base_items|SignatureEncoding|SignablePayload|Digest|ScalarTweak|PublicKey|PublicKeyFormat"
base_items="$base_items|Curve|SignerError|SignerErrorKind|Envelope|Asset|ChainName"
base_items="$base_items|ChainNetwork|ChainNetworks|NetworkKind|Identified|Block|Transactions|MovementKind"
# The crypto names that carry a placement rule; every other crypto:: name is free.
crypto_items='KeyPair'
if [ -n "$dirs" ]; then
  find $dirs -name '*.rs' | sort | BASE_ITEMS="$base_items" CRYPTO_ITEMS="$crypto_items" perl -e '
    use strict; use warnings;
    my $allowed = qr/^(?:$ENV{BASE_ITEMS})$/;
    my $placed = qr/^(?:$ENV{CRYPTO_ITEMS})$/;
    my $status = 0;
    # The first CamelCase segment of a path names the item: base::transaction::Envelope::new.
    sub item { my ($path) = @_; $path =~ s/\s+as\s+\w+$//;
      for (split /::/, $path) { return $_ if /^[A-Z]/ } return undef }
    sub expand { my ($text, $out) = @_; my ($depth, $current, @entries) = (0, "");
      for my $char (split //, $text) {
        $depth++ if $char eq "{"; $depth-- if $char eq "}";
        if ($char eq "," && $depth == 0) { push @entries, $current; $current = "" }
        else { $current .= $char } }
      for my $entry (@entries, $current) { $entry =~ s/^\s+|\s+$//g; next unless length $entry;
        if ($entry =~ /^.*?::\{(.*)\}$/s) { expand($1, $out) }
        else { my $name = item($entry); push @$out, $name if defined $name } } }
    while (my $file = <STDIN>) { chomp $file;
      open my $handle, "<", $file or die "$file: $!"; local $/; my $source = <$handle>;
      $source =~ s{//[^\n]*}{}g;
      my $tests = $source =~ /^\s*#\[cfg\(test\)\]/m ? $-[0] : length $source;
      while ($source =~ /\b(base|crypto)::(\{(?:[^{}]|(?2))*\}|(?:\w+::)*\w+)/g) {
        my ($crate, $path, $at) = ($1, $2, $-[0]); my @names;
        if ($path =~ /^\{(.*)\}$/s) { expand($1, \@names) }
        else { my $name = item($path); push @names, $name if defined $name }
        my $line = 1 + (substr($source, 0, $at) =~ tr/\n//);
        for my $name (@names) {
          next if $crate eq "base" ? $name =~ $allowed : $name !~ $placed;
          next if $name eq "KeyPair" && ($at >= $tests || $file =~ m{/native/key\.rs$});
          print "$file:$line: ${crate}::$name is not a native input\n"; $status = 1; } } }
    exit $status;
  ' || status=1
fi
# 6. Retired names must not come back anywhere in code:
#    - one chain-neutral address (a retired design note (011)): base::Address, no other address
#      type and no BaseAddress alias;
#    - history is one repository query by asset, served by wallets::Wallets
#      (docs/design/030 §8 (ex-012), 014): no wallet-side history reader or post-filter;
#    - one factory concept, wallets::WalletFactory, registered once with
#      Wallets::register (a retired design note (014)): no provider or family.
banned='CanonicalAddress|AddressText|AddressEncoding|AddressFormat|AddressValidator|BaseAddress'
banned="$banned|HistoryReader|IndexedHistory|selected_history|history_asset"
banned="$banned|Provider|WalletProvider|Family|enroll|enroll_with_registry"
#    - one network vocabulary (a retired design note (018)): base::Network is the identity,
#      base::Networked converts to it, and indexing::NetworkRules admits it;
#      no IndexScope, no TestnetMap, no TransactionEnvelope alias;
#    - one meaning per retired chain noun (docs/CHAIN_LAYOUT.md section 3).
banned="$banned|IndexScope|TestnetMap|TransactionEnvelope|MemoSource|UtxoSet|Holding"
#    - the (chain, network) partition is a *network*, never a "scope": the
#      health vocabulary is NetworkState/NetworkFailure/NetworkHealth/
#      NetworkEvent, the handshake is CommitGate, the error kind is
#      IndexErrorKind::NetworkMismatch, and the callback is
#      SyncMonitor::network_changed (docs/CHAIN_LAYOUT.md section 3).
banned="$banned|ScopeState|ScopeFailure|ScopeHealth|ScopeEvent|ScopeAdmission"
banned="$banned|ScopeMismatch|scope_changed"
grep -rnE --include='*.rs' "${w}(${banned})([^A-Za-z0-9_]|$)" sdk apps examples packages && status=1
# 6b. The same retired nouns, spelled in lower case.
#     Check 6 is case-sensitive, and for a type name that is exactly right:
#     `Holding` in prose is the argument for retiring it, `Holding` in code is
#     the regression. But it is blind to the module, function or binding that
#     spells the noun in lower case, and `mod holdings;`, `fn holding(..) ->
#     Subject` and `let holding = Subject {..}` all sat in this tree past it.
#     So a second list, matched case-insensitively: the retired nouns that are
#     also ordinary English words. Only those — a name like `ScopeState` needs
#     no second pass, and `scope` cannot join because `std::thread::scope` owns
#     it (docs/CHAIN_LAYOUT.md section 3, docs/DEFECT_PATTERNS.md section 2.1:
#     a pattern that can be made mechanical should be a rule, not a paragraph).
#
#     Two narrowings keep this a rule about *names*, and both are load-bearing.
#     Comments and string literals are stripped before matching, because prose
#     must stay free to name the noun it is explaining — the doc comments that
#     record why `Holding` was retired say it, and so do assertion messages.
#     And the word boundary is check 6's own, so a snake_case identifier is one
#     word: `holding` is caught, `without_holding_the_admission_lock` is not.
#     The second is not a concession. `holding` there is the verb, and nothing
#     a regex can see separates a verb from a noun inside an identifier.
english='holding|holdings|provider|providers|family|families'
find sdk apps examples packages -name '*.rs' -type f | sort | ENGLISH="$english" perl -e '
  use strict; use warnings;
  my $retired = qr/(?:^|[^A-Za-z0-9_])(?:$ENV{ENGLISH})(?:[^A-Za-z0-9_]|$)/i;
  my $status = 0;
  while (my $file = <STDIN>) { chomp $file;
    open my $handle, "<", $file or die "$file: $!";
    my $line = 0;
    while (my $source = <$handle>) { $line++;
      my $code = $source;
      $code =~ s{//.*}{};
      $code =~ s{"(?:[^"\\]|\\.)*"}{""}g;
      next unless $code =~ $retired;
      chomp $source;
      print "$file:$line: a retired noun names something here:$source\n"; $status = 1; } }
  exit $status;
' || status=1
#     And the file a `mod <name>;` above names, which that check reaches only
#     through the declaration.
find sdk apps examples packages -type f -name '*.rs' | grep -iE "/(${english})\.rs\$" \
  && { echo "a file is named for a retired noun"; status=1; }
# 7. Chain vocabulary (a retired design note (007), docs/CHAIN_LAYOUT.md section 3): a chain
#    crate defines no Parsed*/*Data/Interpreted*/Facts/Movements/Blocks types, no
#    trait Transactions, no Envelope of its own (base::transaction::Envelope),
#    and no public *Wire type. `type Transactions = base::Transactions<_>` is
#    the one allowed Transactions item. Across sdk/, a plural noun is a
#    collection of values, never a trait: storage ports are *Store.
#
#    The second list is the nouns section 3 bans for saying nothing or saying
#    two things at once. They were documented as checked here and were not, and
#    `struct Preparation` sat in bitcoin's outbox past the claim
#    (docs/DEFECT_PATTERNS.md: a pattern that can be made mechanical should be
#    a rule, not a paragraph — and a gate that is documented and absent is
#    worse than no gate). `Entry` is the bare noun only, and its one remedy is
#    `PreparedTransaction`, so `*Entry` is untouched.
#    `Context` cannot join: section 3 bans it only *for a slot-stamped value*
#    (`rpc::Reading<T>`), and nothing a regex can see separates that from the
#    per-block parsing `Context` bitcoin's block reader legitimately has — the
#    same narrowing check 6b makes for `scope`.
item='^[[:space:]]*(pub(\([a-z]+\))?[[:space:]]+)?(struct|enum|type|trait)[[:space:]]+'
grep -rnE --include='*.rs' "${item}(Parsed[A-Za-z0-9_]*|[A-Za-z0-9_]*Data|Interpreted[A-Za-z0-9_]*|Facts|Movements|Blocks|Envelope)([^A-Za-z0-9_]|$)" sdk/chains/*/src && status=1
vocabulary='Coordinator|Preparation|Funding|Entry|Attempt|Evidence|Resolver|IndexerSettings'
grep -rnE --include='*.rs' "${item}(${vocabulary})([^A-Za-z0-9_]|$)" sdk/chains/*/src \
  && { echo "a chain crate declares a name docs/CHAIN_LAYOUT.md section 3 bans"; status=1; }
grep -rnE --include='*.rs' "^[[:space:]]*pub[[:space:]]+(struct|enum|type)[[:space:]]+[A-Za-z0-9_]*Wire([^A-Za-z0-9_]|$)" sdk/chains/*/src && status=1
grep -rnE --include='*.rs' "${w}trait[[:space:]]+(Blocks|Transactions|Outputs)([^A-Za-z0-9_]|$)" sdk apps examples && status=1
# 9. `Source` is the indexer's block source and nothing else
#    (docs/CHAIN_LAYOUT.md section 3): inside a chain crate only src/source.rs
#    may declare one; a sender's payer is a Payer.
if [ -n "$(ls -d sdk/chains/*/src 2>/dev/null)" ]; then
  grep -rnE --include='*.rs' \
    "^[[:space:]]*(pub(\([a-z]+\))?[[:space:]]+)?(struct|enum|type|trait)[[:space:]]+Source([^A-Za-z0-9_]|$)" \
    sdk/chains/*/src | grep -vE '/source\.rs:' \
    && { echo "Source declared outside a chain's src/source.rs"; status=1; }
fi

# 8. Only the composition root names a chain crate (a retired design note (015)). The linter
#    works on Cargo packages, so it cannot see inside apps/server.
#
#    There is one application crate, and it is a binary: apps/server reads its
#    configuration and serves the HTTP API, and exports nothing. The routes were
#    a package of their own (apps/api) while a manifest kept them from naming a
#    chain; they are apps/server/src/api now, so this check is what keeps them:
#    - src/chains.rs is the one file of the application that names a chain
#      crate;
#    - nothing under src/api names a chain crate, the configuration reader
#      (sdk_config), a storage backend (indexing::sqlite, indexing::memory,
#      rusqlite), or the composition root's own chain and storage modules.
#    Text is matched, comments included: a route has no reason to say either.
app=apps/server/src
[ -f "$app/chains.rs" ] || { echo "$app/chains.rs is missing"; status=1; }
[ -d "$app/api" ] || { echo "$app/api is missing"; status=1; }
[ -e apps/api ] && { echo "apps/api exists; the routes are $app/api"; status=1; }
ls apps/*/src/lib.rs >/dev/null 2>&1 && { echo "an application exports a library"; status=1; }
chain_crate="${w}chain_(bitcoin|ethereum|solana)([^A-Za-z0-9_]|\$)|${w}chain_[a-z]+::"
grep -rnE "$chain_crate" "$app" \
  | grep -vE "^$app/chains\.rs:" && { echo "chain crate outside $app/chains.rs"; status=1; }
composed="chain_[a-z]+|sdk_config|rusqlite|indexing::(sqlite|memory)|crate::(chains|storage)"
grep -rnE "${w}(${composed})([^A-Za-z0-9_]|\$)" "$app/api" \
  && { echo "a route names a chain, the configuration reader or a storage backend"; status=1; }
grep -rnE 'enum (Chain|WalletAsset)\b|WalletAsset::' "$app/api" && { echo "closed asset/chain enum in routes"; status=1; }

# 10. The lenient address parse is store-free and there is exactly one of it
#     (docs/CHAIN_LAYOUT.md section 3, Addresses). `wallets::AddressParser` is
#     implemented by `WalletConfig` — a value any caller builds from a `Network`
#     in plain code — so validating user-supplied address text needs no store,
#     no node and no index, and a third party embedding this SDK over its own
#     repository never falls back to `base::Address::new` and trusts itself.
#     The factory keeps its own impl because `wallets::WalletFactory` requires
#     it, and that impl must only delegate: two parses that can disagree are
#     worse than one awkward parse, and the delegation is what makes them one.
for file in sdk/chains/*/src/wallet.rs; do
  [ -f "$file" ] || { echo "no wallet.rs under sdk/chains/*/src"; status=1; break; }
  grep -qE '^impl AddressParser for WalletConfig[[:space:]]*\{' "$file" \
    || { echo "$file: WalletConfig must implement wallets::AddressParser"; status=1; }
  grep -qE '^[[:space:]]*self\.config\.parse_address\(text\)$' "$file" \
    || { echo "$file: AddressParser for WalletFactory must delegate to WalletConfig"; status=1; }
done

# 4. Every scenario configures every chain (004); the linter cannot correlate
#    directories. A scenario is one directory with one main.rs and the
#    config.toml beside it that states its chains, one `[<chain>]` section per
#    chain crate. The scenario list is the directories themselves, so a new
#    scenario is covered the moment it exists; examples/tests/scenarios.rs
#    additionally checks that each main.rs registers exactly the chains its
#    config.toml configures, each under the name its section uses; custom_store
#    registers none, because it assembles its index by hand.
#    An unexpanded glob would make `basename` report a chain named "*" and pass
#    silently, so a missing sdk/chains/ is a failure, not a skip.
if [ "$CHECK_EXAMPLES" = 1 ]; then
  if [ ! -d sdk/chains ]; then
    echo "sdk/chains is missing; cannot check the scenario examples"
    status=1
  fi
  scenarios=$(ls -d examples/*/ 2>/dev/null | grep -v '^examples/tests/$')
  [ -n "$scenarios" ] && [ -d sdk/chains ] || { echo "no scenario directory to check"; status=1; }
  for scenario in $scenarios; do
    file="${scenario}config.toml"
    [ -f "${scenario}main.rs" ] || { echo "missing ${scenario}main.rs"; status=1; continue; }
    [ -f "$file" ] || { echo "missing $file"; status=1; continue; }
    for dir in sdk/chains/*/; do
      [ -d "$dir" ] || { echo "no chain crate under sdk/chains"; status=1; break; }
      chain=$(basename "$dir")
      grep -qE "^\[$chain\]" "$file" || { echo "$file does not configure $chain"; status=1; }
    done
  done
fi

# 11. A module directory holds more than one file.
#    A folder containing exactly one `.rs` file is not a boundary: it says a
#    sibling is coming that never came. Fold the file into the parent module's
#    own file (`<parent>.rs`) and drop the directory; what was `pub(super)`
#    becomes private there. Do not reach for a `#[path]` attribute: loading a
#    file under another module's name is an include trick (AGENTS.md), and the
#    ones the indexing engine had were removed for that reason. Cargo owns
#    `tests/`, `examples/` and `benches/`, whose one-file shape it mandates, so
#    only `src/` is checked.
for dir in $(find sdk apps packages -type d -path '*/src/*' 2>/dev/null); do
  case "$dir" in *"/target/"*) continue ;; esac
  files=$(find "$dir" -mindepth 1 -maxdepth 1 -name '*.rs' | wc -l | tr -d ' ')
  subs=$(find "$dir" -mindepth 1 -maxdepth 1 -type d | wc -l | tr -d ' ')
  if [ "$files" = "1" ] && [ "$subs" = "0" ]; then
    echo "$dir holds one file and no subdirectory; fold it into its parent module"
    status=1
  fi
done

# 12. Nothing imports a store (ARCHITECTURE.md, "Dependency direction").
#     sdk/indexing/src/storage/ holds the ports and what every store shares, in
#     the files directly in it; each directory under it is one store. A store
#     may import the shared files and the engine. Nothing imports a store: not
#     the engine (src/ and src/supervisor/), not a shared file directly in
#     src/storage/, and not another store. `rusqlite` is the `sqlite` store's
#     own crate and is held the same way.
#
#     It lives here because the linter has no rule for it: `rust/layers` reads
#     Cargo dependency edges between packages, not `use` paths, and no other
#     rule sees a module boundary inside one crate. The shape of the directory
#     -- every folder a store, its root file inside it -- is a rule about
#     paths, so that part is `layout` in linter.toml.
#
#     The stores are the directories themselves, so a new one is covered the
#     moment it exists, and none at all is a failure rather than a pass. An
#     import is a path into the module (`sqlite::`) or its name in a `use`.
#     Declaring a module is not depending on it: `pub mod sqlite;` in
#     storage.rs is neither, and one statement is exempt, matched whole and
#     only in lib.rs -- `pub use storage::<store>;`, which is what makes
#     `indexing::<store>` reachable for a caller. Comments and string literals
#     are blanked first, so prose may name a store and
#     `#[cfg(feature = "sqlite")]` is a gate, not an import. An item under
#     `#[cfg(test)]` is blanked too -- that item, not the rest of the file --
#     and tests/ is never read: contract suites exist to hold both stores.
index=sdk/indexing/src
stores=$(find "$index/storage" -mindepth 1 -maxdepth 1 -type d 2>/dev/null \
  | sed 's|.*/||' | sort | tr '\n' ' ')
if [ -z "$stores" ]; then
  echo "no store directory under $index/storage; cannot check what imports one"
  status=1
else
  find "$index" -name '*.rs' -type f | sort | INDEX="$index" STORES="$stores" perl -e '
    use strict; use warnings;
    my $index = $ENV{INDEX};
    my %owner = map { $_ => $_ } split " ", $ENV{STORES};
    $owner{rusqlite} = "sqlite" if $owner{sqlite};
    my $status = 0;
    sub blank { my ($text) = @_; $text =~ s/[^\n]/ /g; $text }
    while (my $file = <STDIN>) { chomp $file;
      my $own = $file =~ m{^\Q$index\E/storage/([^/]+)/} ? $1 : "";
      my $names = join "|", grep { $owner{$_} ne $own } sort keys %owner;
      next unless length $names;
      open my $handle, "<", $file or die "$file: $!"; local $/; my $source = <$handle>;
      my $code = $source;
      # One pass over comments, strings and characters, so none can open another.
      $code =~ s{(//[^\n]*|/\*.*?\*/|r(#*)".*?"\2|"(?:[^"\\]|\\.)*"|\x27(?:\\.|[^\\\x27\n])\x27)}
        {blank($1)}gse;
      # A test-only item: through its first `;`, or to the end of the block it opens.
      while ($code =~ /#\[cfg\((?:all\()?test\b[^\]]*\]/g) {
        my ($from, $at, $depth) = ($-[0], $+[0], 0);
        while ($at < length $code) {
          my $char = substr $code, $at++, 1;
          $depth++ if $char eq "{";
          last if $char eq "}" && --$depth <= 0;
          last if $char eq ";" && $depth == 0;
        }
        substr($code, $from, $at - $from) = blank(substr $code, $from, $at - $from);
        pos($code) = $at;
      }
      $code =~ s{(\bpub\s+use\s+(?:(?:crate|self)::)?storage::(?:$names)\s*;)}{blank($1)}ge
        if $file eq "$index/lib.rs";
      my %found;
      $found{$-[1]} = $1 while $code =~ /(?<![A-Za-z0-9_])($names)\s*::/g;
      while ($code =~ /\buse\s([^;]*);/g) { my ($body, $from) = ($1, $-[1]);
        $found{$from + $-[1]} = $1 while $body =~ /(?<![A-Za-z0-9_])($names)(?![A-Za-z0-9_])/g; }
      my @lines = split /\n/, $source; my %reported;
      for my $at (sort { $a <=> $b } keys %found) {
        my $line = 1 + (substr($code, 0, $at) =~ tr/\n//);
        next if $reported{$line}++;
        print "$file:$line: $found{$at} is imported outside $index/storage/$owner{$found{$at}}:",
          " $lines[$line - 1]\n";
        $status = 1; } }
    exit $status;
  ' || status=1
fi

exit "$status"
