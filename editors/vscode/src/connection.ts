// How this extension reaches Ferridis, parsed once from settings.
//
// Pure: no `vscode` import, so the rules below run under `node --test`
// without an editor. extension.ts reads raw settings, calls
// `parseConnection`, and acts only on the variant it gets back.

/** A service endpoint that has passed the checks in `parseServiceUrl`. */
export type ServiceUrl = { readonly kind: "service-url"; readonly href: string };

/** A bearer token that is safe to place in an HTTP header. */
export type BearerToken = { readonly kind: "bearer-token"; readonly value: string };

/** Where Ferridis tools come from. Exactly one variant holds at a time. */
export type Connection =
    /** Connect to an already-running ferridis-mcp-server over Streamable HTTP. */
    | { readonly kind: "service"; readonly url: ServiceUrl }
    /** Launch a private ferridis-mcp-server over stdio (the v0.2 behaviour). */
    | {
          readonly kind: "stdio";
          readonly serverPath: string;
          readonly adaptersConfig: string;
          readonly keychainNamespace: string;
      }
    /** Neither `ferridis.serverUrl` nor `ferridis.serverPath` is set. */
    | { readonly kind: "unconfigured" };

/** Why a setting was refused. Each variant names the offending value. */
export type ConnectionError =
    | { readonly kind: "invalid-url"; readonly raw: string }
    | { readonly kind: "unsupported-scheme"; readonly scheme: string }
    | { readonly kind: "cleartext-to-remote-host"; readonly host: string };

/** Why a token was refused. Never carries the token itself. */
export type TokenError =
    | { readonly kind: "empty" }
    | { readonly kind: "contains-whitespace" };

export type Result<T, E> =
    | { readonly ok: true; readonly value: T }
    | { readonly ok: false; readonly error: E };

/** The settings as VS Code hands them over, before any checks. */
export type RawSettings = {
    readonly serverUrl: string;
    readonly serverPath: string;
    readonly adaptersConfig: string;
    readonly keychainNamespace: string;
};

const LOOPBACK_HOSTS: ReadonlySet<string> = new Set([
    "127.0.0.1",
    "localhost",
    "[::1]",
]);

/**
 * Accept https anywhere, and http only to a loopback host: the bearer
 * token travels in a header, so cleartext to another machine would
 * publish it.
 */
export function parseServiceUrl(raw: string): Result<ServiceUrl, ConnectionError> {
    let url: URL;
    try {
        url = new URL(raw);
    } catch {
        return { ok: false, error: { kind: "invalid-url", raw } };
    }
    if (url.protocol === "https:") {
        return { ok: true, value: { kind: "service-url", href: url.href } };
    }
    if (url.protocol !== "http:") {
        return { ok: false, error: { kind: "unsupported-scheme", scheme: url.protocol } };
    }
    if (!LOOPBACK_HOSTS.has(url.hostname)) {
        return { ok: false, error: { kind: "cleartext-to-remote-host", host: url.hostname } };
    }
    return { ok: true, value: { kind: "service-url", href: url.href } };
}

/**
 * `serverUrl` wins over `serverPath` when both are set, so a user moving to
 * the service does not also have to clear the old setting.
 */
export function parseConnection(
    raw: RawSettings,
    defaultAdaptersConfig: string,
): Result<Connection, ConnectionError> {
    const serverUrl = raw.serverUrl.trim();
    if (serverUrl !== "") {
        const url = parseServiceUrl(serverUrl);
        return url.ok ? { ok: true, value: { kind: "service", url: url.value } } : url;
    }
    const serverPath = raw.serverPath.trim();
    if (serverPath === "") {
        return { ok: true, value: { kind: "unconfigured" } };
    }
    return {
        ok: true,
        value: {
            kind: "stdio",
            serverPath,
            adaptersConfig: raw.adaptersConfig.trim() || defaultAdaptersConfig,
            keychainNamespace: raw.keychainNamespace.trim() || "ferridis",
        },
    };
}

/**
 * Surrounding whitespace is trimmed (a pasted token often ends in a
 * newline); whitespace inside is refused, because a CR or LF inside a
 * header value would split the header.
 */
export function parseBearerToken(raw: string): Result<BearerToken, TokenError> {
    const value = raw.trim();
    if (value === "") {
        return { ok: false, error: { kind: "empty" } };
    }
    if (/\s/.test(value)) {
        return { ok: false, error: { kind: "contains-whitespace" } };
    }
    return { ok: true, value: { kind: "bearer-token", value } };
}

/** One line a user can act on. Never includes a token. */
export function describeConnectionError(error: ConnectionError): string {
    switch (error.kind) {
        case "invalid-url":
            return `ferridis.serverUrl is not a valid URL: "${error.raw}"`;
        case "unsupported-scheme":
            return `ferridis.serverUrl must use http or https, not ${error.scheme}`;
        case "cleartext-to-remote-host":
            return `ferridis.serverUrl uses http to ${error.host}; use https, or a loopback address, so the token is not sent in clear text`;
    }
}

export function describeTokenError(error: TokenError): string {
    switch (error.kind) {
        case "empty":
            return "The token is empty.";
        case "contains-whitespace":
            return "The token contains whitespace, which is not valid in a bearer token.";
    }
}
