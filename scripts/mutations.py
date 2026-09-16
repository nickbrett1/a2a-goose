#!/usr/bin/env python3
"""Break the code on purpose and check that the tests notice.

A test that passes is not evidence of anything until it has been shown to fail
when the thing it describes is removed. Each case below is a small, deliberate
mutation of the source -- adopt a goose somebody else started, skip the
readiness gate, leave the key out, `SIGKILL` instead of asking -- paired with the
test that is supposed to catch it. The test must fail; if it does not, the test
is decoration and this exits non-zero.

Run from the repository root, with `cargo` on PATH:

    python3 scripts/mutations.py

Every file it edits is restored from its original bytes afterwards, including
when a case raises, so a failed run leaves the tree as it found it. Mutations are
textual on purpose: if the patterns below stop matching, that is itself worth
knowing, and the run reports `PATTERN MISSING` rather than a quiet pass.
"""
import subprocess
import sys

# (what the mutation removes, file, exact text, replacement, test that must fail)
CASES = [
    (
        "adopt whatever is already serving on the ACP address",
        "src/serve.rs",
        "        if let Some(detail) = occupied(&address, &acp).await {",
        "        if let Some(detail) = None::<String> {",
        "an_address_somebody_is_already_using_is_a_refusal_and_nothing_is_spawned",
    ),
    (
        "no readiness gate: call the child up as soon as it has spawned",
        "src/serve.rs",
        "    match wait_ready(&mut child, &acp, prober.as_ref(), &mut stop).await {",
        "    match Readiness::Ready {",
        "a_goose_that_comes_up_mute_is_stopped_rather_than_left_holding_the_port",
    ),
    (
        "SIGKILL the child instead of asking it to stop",
        "src/serve.rs",
        "        if let Err(err) = signal_term(pid) {",
        "        if let Err(err) = Ok::<(), std::io::Error>(()) {",
        "a_goose_that_comes_up_mute_is_stopped_rather_than_left_holding_the_port",
    ),
    (
        "never give up: restart a child that will not stay up, forever",
        "src/serve.rs",
        "        self.at.len() as u32 > self.burst",
        "        self.at.len() as u32 > u32::MAX",
        "a_goose_that_will_not_stay_up_is_given_up_on",
    ),
    (
        "spawn goose with no key, as if one were not needed",
        "src/serve.rs",
        "            (None, false) => {",
        "            (None, false) if false => {",
        "a_goose_that_cannot_be_started_at_all_says_what_is_missing",
    ),
    (
        "forget that the ACP connection died",
        "src/acp/transport.rs",
        "                dead.store(true, Ordering::Relaxed);",
        "                let _ = &dead;",
        "the_end_of_the_connection_stream_is_what_says_the_connection_is_gone",
    ),
    (
        "let `own` accept an address it could not start a server on",
        "src/config.rs",
        "            return Err(ConfigError::OwnedAcpNotLoopback { url: url.clone() });",
        "            let _ = &url;",
        "an_owned_goose_refuses_a_url_it_could_not_start_a_server_on",
    ),
]


def main() -> int:
    originals = {}
    for _, path, _, _, _ in CASES:
        originals.setdefault(path, open(path).read())

    results = []
    try:
        for name, path, find, repl, test in CASES:
            src = originals[path]
            if find not in src:
                results.append((name, test, "PATTERN MISSING"))
                continue
            open(path, "w").write(src.replace(find, repl, 1))
            # A substring filter, not `--exact`: the lib tests are named
            # `config::tests::...` and the integration tests are not.
            proc = subprocess.run(
                ["cargo", "test", "--locked", "--", test],
                capture_output=True,
                text=True,
            )
            out = proc.stdout + proc.stderr
            if "error[E" in out or "error: could not compile" in out:
                verdict = "DID NOT BUILD"
            elif proc.returncode == 0:
                verdict = "MISSED"
            else:
                verdict = "CAUGHT"
            results.append((name, test, verdict))
            open(path, "w").write(src)
    finally:
        for path, src in originals.items():
            open(path, "w").write(src)

    print()
    for name, test, verdict in results:
        print(f"{verdict:15} {name}")
        print(f"{'':15} [{test}]")

    bad = [r for r in results if r[2] != "CAUGHT"]
    print()
    if bad:
        print(f"{len(bad)} of {len(results)} mutation(s) not caught")
        return 1
    print(f"all {len(results)} mutations caught")
    return 0


if __name__ == "__main__":
    sys.exit(main())
