(function (request, output, outputImage, done, namesJson, maxConcurrentCalls) {
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
    const text = value => { if (!finished) output(typeof value === "string" ? value : String(stringify(value))); };
    const console = Object.create(null);
    for (const level of ["log", "info", "warn", "error", "debug"]) {
        console[level] = (...args) => text(args.map(value => typeof value === "string" ? value : stringify(value)).join(" "));
    }
    Object.defineProperty(globalThis, "console", {value: Object.freeze(console)});
    Object.defineProperty(globalThis, "exit", {value: () => { finished = true; done(null); throw EXIT; }});
    Object.defineProperty(globalThis, "tools", {value: Object.freeze(tools)});
    Object.defineProperty(globalThis, "text", {value: text});
    Object.defineProperty(globalThis, "image", {value: value => { if (!finished) outputImage(stringify(value)); }});
    return {
        run(fn) {
            fn().then(value => {
                if (!finished) {
                    if (value !== undefined) text(value);
                    finished = true;
                    done(null);
                }
            }, error => { if (!finished) { finished = true; done(String(error)); } });
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
