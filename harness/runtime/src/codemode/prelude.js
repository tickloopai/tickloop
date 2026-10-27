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
    // The script shares this realm and may replace builtins, so store state lives in
    // prototype-free records reached only through syntax and functions captured here; the host
    // checks the reported writes again.
    const ownKeys = Object.keys;
    const apply = Reflect.apply;
    const regexpTest = RegExp.prototype.test;
    // JSON.stringify escapes only unpaired surrogates, which the host's JSON cannot hold.
    const UNPAIRED_SURROGATE = /(?:^|[^\\])(?:\\\\)*\\ud[89a-f][0-9a-f]{2}/;
    const STORE_HINT = "store() is for small state such as IDs, cursors or summaries; keep large data in variables or write it to a file with a tool.";
    const stored = Object.create(null);
    const writes = Object.create(null);
    const storeState = parse(storeJson);
    const initialStore = storeState.values;
    const MAX_STORE_VALUE_CHARS = storeState.maxValueChars;
    const MAX_STORE_TOTAL_CHARS = storeState.maxTotalChars;
    const MAX_STORE_KEYS = storeState.maxKeys;
    let storedChars = 0;
    let storedKeys = 0;
    const initialKeys = ownKeys(initialStore);
    for (let index = 0; index < initialKeys.length; index++) {
        const key = initialKeys[index];
        stored[key] = initialStore[key];
        storedChars += key.length + stored[key].length;
        storedKeys++;
    }
    function checkKey(name, key) {
        if (typeof key !== "string") throw new TypeError(name + "() key must be a string");
    }
    function store(key, value) {
        checkKey("store", key);
        const exists = key in stored;
        const previous = exists ? key.length + stored[key].length : 0;
        if (value === undefined) {
            if (exists) {
                delete stored[key];
                storedChars -= previous;
                storedKeys--;
            }
            writes[key] = undefined;
            return;
        }
        let json;
        try { json = stringify(value); } catch (error) { json = undefined; }
        if (typeof json !== "string") throw new TypeError("store(" + stringify(key) + ") value is not JSON-serializable");
        if (apply(regexpTest, UNPAIRED_SURROGATE, [json])) throw new TypeError("store(" + stringify(key) + ") value contains an unpaired surrogate");
        if (json.length > MAX_STORE_VALUE_CHARS) {
            throw new RangeError("store(" + stringify(key) + ") value has " + json.length + " characters of JSON, more than the limit of " + MAX_STORE_VALUE_CHARS + ". " + STORE_HINT);
        }
        const next = storedChars - previous + key.length + json.length;
        if (next > MAX_STORE_TOTAL_CHARS) {
            throw new RangeError("store is full: keys and values would exceed " + MAX_STORE_TOTAL_CHARS + " characters of JSON. Delete keys with store(key, undefined). " + STORE_HINT);
        }
        if (!exists && storedKeys >= MAX_STORE_KEYS) {
            throw new RangeError("store is full: it holds at most " + MAX_STORE_KEYS + " keys. Delete keys with store(key, undefined). " + STORE_HINT);
        }
        if (!exists) storedKeys++;
        stored[key] = json;
        storedChars = next;
        writes[key] = json;
    }
    function load(key) {
        checkKey("load", key);
        return key in stored ? parse(stored[key]) : undefined;
    }
    function succeed() {
        let report = "[";
        let separator = "";
        const keys = ownKeys(writes);
        for (let index = 0; index < keys.length; index++) {
            const key = keys[index];
            const json = writes[key];
            report += separator + "[" + stringify(key) + (json === undefined ? "" : "," + stringify(json)) + "]";
            separator = ",";
        }
        finished = true;
        done(null, report + "]");
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
