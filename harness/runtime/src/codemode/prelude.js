(function (request, output, outputImage, done, namesJson, maxConcurrentCalls, storeJson) {
    "use strict";
    const parse = JSON.parse;
    const stringify = JSON.stringify;
    const pending = new Map();
    const queued = [];
    let active = 0;
    function pump() {
        while (!finished && active < maxConcurrentCalls && queued.length) {
            const [id, name, json] = queued.shift();
            active++;
            request(id, name, json);
        }
    }
    let nextId = 0;
    let finished = false;
    const EXIT = Object.freeze({});
    const tools = Object.create(null);
    const names = parse(namesJson);
    const aliases = new Map();
    for (const name of names) {
        const identifier = name.replace(/[^A-Za-z0-9_$]/g, "_").replace(/^[0-9]/, "_");
        aliases.set(identifier, aliases.has(identifier) ? null : name);
        tools[name] = (input) => new Promise((resolve, reject) => {
            if (finished) { reject(new Error("Script already ended")); return; }
            if (!input || typeof input !== "object" || Array.isArray(input)) {
                reject(new Error("Tool arguments must be an object"));
                return;
            }
            const json = stringify(input);
            const id = ++nextId;
            pending.set(id, {resolve, reject});
            queued.push([id, name, json]);
            pump();
        });
    }
    for (const [identifier, name] of aliases) {
        if (Object.hasOwn(tools, identifier)) continue;
        if (name !== null) tools[identifier] = tools[name];
        else Object.defineProperty(tools, identifier, {get() {throw new Error("Ambiguous tool identifier; use tools['original-name']");}});
    }
    const MAX_STORE_VALUE_CHARS = 262144;
    const MAX_STORE_TOTAL_CHARS = 1048576;
    const STORE_HINT = "store() is for small state such as IDs, cursors or summaries; keep large data in variables or write it to a file with a tool.";
    const stored = new Map();
    for (const [key, value] of Object.entries(parse(storeJson))) stored.set(key, stringify(value));
    const writes = new Map();
    let storedChars = 0;
    for (const [key, json] of stored) storedChars += key.length + json.length;
    function checkKey(name, key) {
        if (typeof key !== "string") throw new TypeError(name + "() key must be a string");
    }
    function store(key, value) {
        checkKey("store", key);
        const previous = stored.has(key) ? key.length + stored.get(key).length : 0;
        if (value === undefined) {
            stored.delete(key);
            storedChars -= previous;
            writes.set(key, undefined);
            return;
        }
        let json;
        try { json = stringify(value); } catch (error) { json = undefined; }
        if (typeof json !== "string") throw new TypeError("store(" + stringify(key) + ") value is not JSON-serializable");
        if (json.length > MAX_STORE_VALUE_CHARS) {
            throw new RangeError("store(" + stringify(key) + ") value has " + json.length + " characters of JSON, more than the limit of " + MAX_STORE_VALUE_CHARS + ". " + STORE_HINT);
        }
        const next = storedChars - previous + key.length + json.length;
        if (next > MAX_STORE_TOTAL_CHARS) {
            throw new RangeError("store is full: keys and values would exceed " + MAX_STORE_TOTAL_CHARS + " characters of JSON. Delete keys with store(key, undefined). " + STORE_HINT);
        }
        stored.set(key, json);
        storedChars = next;
        writes.set(key, json);
    }
    function load(key) {
        checkKey("load", key);
        const json = stored.get(key);
        return json === undefined ? undefined : parse(json);
    }
    function succeed() {
        const entries = [];
        for (const [key, json] of writes) entries.push(json === undefined ? [key] : [key, json]);
        finished = true;
        done(null, stringify(entries));
    }
    const text = value => { if (!finished) output(typeof value === "string" ? value : String(stringify(value))); };
    const console = Object.create(null);
    for (const level of ["log", "info", "warn", "error", "debug"]) {
        console[level] = (...args) => text(args.map(value => typeof value === "string" ? value : stringify(value)).join(" "));
    }
    Object.defineProperty(globalThis, "console", {value: Object.freeze(console)});
    Object.defineProperty(globalThis, "exit", {value: () => { succeed(); throw EXIT; }});
    Object.defineProperty(globalThis, "store", {value: store});
    Object.defineProperty(globalThis, "load", {value: load});
    Object.defineProperty(globalThis, "tools", {value: Object.freeze(tools)});
    Object.defineProperty(globalThis, "text", {value: text});
    Object.defineProperty(globalThis, "image", {value: value => { if (!finished) outputImage(stringify(value)); }});
    return {
        run(fn) {
            fn().then(value => {
                if (!finished) {
                    if (value !== undefined) text(value);
                    succeed();
                }
            }, error => { if (!finished) { finished = true; done(String(error), null); } });
        },
        settle(id, ok, payload) {
            const entry = pending.get(id);
            pending.delete(id);
            active--;
            pump();
            if (ok) entry.resolve(parse(payload));
            else entry.reject(new Error(payload));
        }
    };
})
