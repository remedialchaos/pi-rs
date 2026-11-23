# Temporary upstream oracle

The recipe materializes this block only while rust-anything needs the TypeScript source.

```typescript
import { BACKGROUND_CONTEXT, withAbortSignal } from "../../../packages/chord/src/context/index.ts";
import { Connection } from "../../../packages/env/src/connection.ts";
import { LocalExecutionEnv } from "../../../packages/env/src/list-files.ts";
import { RemoteExecutionEnv } from "../../../packages/env/src/remote-env.ts";

export async function runListing(remote: boolean): Promise<unknown> {
    const cwd = "/";
    const connection = new Connection({ command: [daemon] });
    try {
        const env = remote
            ? new RemoteExecutionEnv({ connection, cwd, id: "pi-env:proof" })
            : new LocalExecutionEnv({ connection, cwd });
        const aborted = withAbortSignal(AbortSignal.abort(), BACKGROUND_CONTEXT);
        const results = [];
        for (const [path, maxEntries, context] of [
            [fixture, undefined, BACKGROUND_CONTEXT],
            [fixture, 1, BACKGROUND_CONTEXT],
            ["/etc/hosts", undefined, BACKGROUND_CONTEXT],
            [".", 2 ** 53, BACKGROUND_CONTEXT],
            [".", undefined, aborted],
        ] as const) {
            const result = await env.listFiles(path, { maxEntries }, context);
            if (result.ok) results.push({ ok: true, value: result.value });
            else {
                const { code, message, path } = result.error;
                results.push({ ok: false, error: { code, message, path } });
            }
        }
        return results;
    } finally {
        connection.close();
    }
}

const fixture = process.env.LISTING_FIXTURE;
const daemon = process.env.PI_ENV_DAEMON;
if (!fixture || !daemon) throw new Error("missing listing proof environment");
```
