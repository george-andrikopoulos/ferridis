// Run with `npm test` (node --test; Node strips the type annotations).

import { test } from "node:test";
import assert from "node:assert/strict";
import { parseBearerToken, parseConnection, parseServiceUrl } from "./connection.ts";
import type { RawSettings } from "./connection.ts";

const DEFAULT_ADAPTERS = "/home/u/.ferridis/adapters.json";

function settings(overrides: Partial<RawSettings>): RawSettings {
    return { serverUrl: "", serverPath: "", adaptersConfig: "", keychainNamespace: "", ...overrides };
}

test("http to a loopback host is accepted", () => {
    for (const raw of [
        "http://127.0.0.1:7820/mcp",
        "http://localhost:7820/mcp",
        "http://[::1]:7820/mcp",
    ]) {
        assert.equal(parseServiceUrl(raw).ok, true, raw);
    }
});

test("http to any other host is refused, because the token would travel in clear text", () => {
    for (const raw of ["http://192.168.1.10:7820/mcp", "http://example.com/mcp", "http://127.0.0.2/mcp"]) {
        const r = parseServiceUrl(raw);
        assert.equal(r.ok, false, raw);
        assert.equal(!r.ok && r.error.kind, "cleartext-to-remote-host", raw);
    }
});

test("https is accepted for any host", () => {
    assert.equal(parseServiceUrl("https://ferridis.example.com/mcp").ok, true);
});

test("non-http schemes and unparseable text are refused with distinct reasons", () => {
    const ftp = parseServiceUrl("ftp://127.0.0.1/mcp");
    assert.equal(!ftp.ok && ftp.error.kind, "unsupported-scheme");
    const junk = parseServiceUrl("not a url");
    assert.equal(!junk.ok && junk.error.kind, "invalid-url");
});

test("serverUrl takes precedence over serverPath", () => {
    const r = parseConnection(settings({ serverUrl: "http://127.0.0.1:7820/mcp", serverPath: "C:/ferridis-mcp-server.exe" }), DEFAULT_ADAPTERS);
    assert.equal(r.ok && r.value.kind, "service");
});

test("an invalid serverUrl is an error, never a silent fall-back to stdio", () => {
    const r = parseConnection(settings({ serverUrl: "http://10.0.0.5/mcp", serverPath: "C:/ferridis-mcp-server.exe" }), DEFAULT_ADAPTERS);
    assert.equal(r.ok, false);
});

test("serverPath alone keeps the v0.2 stdio behaviour, with its defaults", () => {
    const r = parseConnection(settings({ serverPath: "  C:/ferridis-mcp-server.exe " }), DEFAULT_ADAPTERS);
    assert.deepEqual(r.ok && r.value, {
        kind: "stdio",
        serverPath: "C:/ferridis-mcp-server.exe",
        adaptersConfig: DEFAULT_ADAPTERS,
        keychainNamespace: "ferridis",
    });
});

test("nothing set is unconfigured", () => {
    const r = parseConnection(settings({ serverUrl: "  " }), DEFAULT_ADAPTERS);
    assert.equal(r.ok && r.value.kind, "unconfigured");
});

test("a pasted token loses its trailing newline", () => {
    const r = parseBearerToken("abc123\r\n");
    assert.equal(r.ok && r.value.value, "abc123");
});

test("a token with whitespace inside is refused, so it cannot split the header", () => {
    for (const raw of ["abc\r\nX-Injected: 1", "abc def", "abc\tdef"]) {
        const r = parseBearerToken(raw);
        assert.equal(!r.ok && r.error.kind, "contains-whitespace", JSON.stringify(raw));
    }
});

test("an empty or blank token is refused", () => {
    const r = parseBearerToken("   ");
    assert.equal(!r.ok && r.error.kind, "empty");
});
