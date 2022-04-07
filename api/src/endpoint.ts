// SPDX-FileCopyrightText: © 2021 ChiselStrike <info@chiselstrike.com>

/// <reference lib="deno.core" />
/// <reference lib="dom" />
/// <reference lib="deno.unstable" />

const endpointWorker = new Worker("file:///worker.js", {
    type: "module",
    name: "endpointWorker",
    deno: {
        namespace: true,
    },
});
type Resolver = {
    resolve: (value: unknown) => void;
    reject: (err: Error) => void;
    msg: unknown;
};
const resolvers: Resolver[] = [];
endpointWorker.onmessageerror = function (e) {
    throw e;
};
endpointWorker.onerror = function (e) {
    throw e;
};
endpointWorker.onmessage = function (event) {
    const resolver = resolvers[0];
    const d = event.data;
    const e = d.err;
    if (e) {
        resolver.reject(e);
    } else {
        resolver.resolve(d.value);
    }
};

Deno.core.setPromiseRejectCallback((type, promise, reason) => {
    console.error("BAD PROMISE", type, promise, reason, new Error().stack);
    Deno.core.opSync("op_chisel_internal_error");
});

async function toWorker(msg: unknown) {
    const p = new Promise((resolve, reject) => {
        resolvers.push({ resolve, reject, msg });
    });
    // Each worker should handle a single request at a time, so we
    // only post a message if the worker is not currently
    // busy. Otherwise we leave it scheduled and know it will be
    // posted once the preceding messages are answered.
    if (resolvers.length == 1) {
        endpointWorker.postMessage(resolvers[0].msg);
    }
    try {
        return await p;
    } finally {
        resolvers.shift();
        // If a message was scheduled while the worker was busy, post
        // it now.
        if (resolvers.length != 0) {
            endpointWorker.postMessage(resolvers[0].msg);
        }
    }
}

export async function initWorker(id: number) {
    await toWorker({ cmd: "initWorker", id });
}

export async function readWorkerChannel() {
    await toWorker({ cmd: "readWorkerChannel" });
}

export async function importEndpoint(
    path: string,
    apiVersion: string,
    version: number,
) {
    await toWorker({
        cmd: "importEndpoint",
        path,
        apiVersion,
        version,
    });
}

export async function activateEndpoint(path: string) {
    await toWorker({
        cmd: "activateEndpoint",
        path,
    });
}

export function endOfRequest(id: number) {
    endpointWorker.postMessage({ cmd: "endOfRequest", id });
}

export async function callHandler(
    path: string,
    apiVersion: string,
    id: number,
) {
    const res = await toWorker({
        cmd: "callHandler",
        path,
        apiVersion,
        id,
    }) as { body?: number; status: number; headers: number };

    // The read function is called repeatedly until it return
    // undefined. In the current implementation it returns the full
    // body on the first call and undefined on the second.
    let body = res.body;
    const read = function () {
        const ret = body;
        body = undefined;
        return ret;
    };
    return {
        "status": res.status,
        "headers": res.headers,
        "read": read,
    };
}
