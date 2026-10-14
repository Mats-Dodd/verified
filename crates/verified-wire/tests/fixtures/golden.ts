/**
 * Writes the golden transcript of every session stream fixture
 * (`sessions/<name>.turns.json`), computed by the AI SDK itself
 * (`readUIMessageStream` from `ai@7.0.127`), and `fix-json.json`, the AI SDK's
 * `fixJson` applied to every prefix of a few documents. A port of q's
 * `server/test/support/Golden.ts` without Effect.
 *
 * Run from a checkout with `ai@7.0.127` installed (q's server has it):
 *   AI=~/Documents/code/q/server/node_modules/ai bun golden.ts
 * from this directory.
 */
import { readdirSync, readFileSync, writeFileSync } from "node:fs"
import { join } from "node:path"

const ai = process.env.AI ?? `${process.env.HOME}/Documents/code/q/server/node_modules/ai`
const { readUIMessageStream } = await import(`${ai}/dist/index.js`)
const { fixJson } = await import(`${ai}/src/util/fix-json.ts`)

type Chunk = { type: string; id?: string; data?: any; errorText?: string }

const replyOf = async (chunks: Chunk[]) => {
  const stream = new ReadableStream({
    start(controller) {
      for (const chunk of chunks) controller.enqueue(chunk)
      controller.close()
    }
  })
  let last = null
  for await (const message of readUIMessageStream({ stream })) last = message
  return last
}

// An actor's transcript: a turn per `data-turn-start`, the envelopes its
// steps carried (each with the step that carried it), and the reply the AI
// SDK makes of the turn's other chunks. Mail waits from `data-envelope` (or a
// task's end, its own notification) until a `data-step-carries` takes it.
const ended: Record<string, string> = { ok: "ok", failed: "failed", "not-started": "not started", unknown: "unknown" }

const transcriptOf = async (chunks: Chunk[]) => {
  const turns: { id: string; inputs: any[]; steps: number; chunks: Chunk[]; errors: string[]; end: string | null }[] = []
  const queued: any[] = []
  for (const chunk of chunks) {
    if (chunk.type === "data-envelope") {
      const { envelope, from, replyTo, text } = chunk.data
      if (text !== undefined) queued.push(replyTo === undefined ? { envelope, from, text } : { envelope, from, replyTo, text })
      continue
    }
    if (chunk.type === "data-task-start") continue
    if (chunk.type === "data-task-end") {
      queued.push({ envelope: { task: chunk.id }, from: "v", text: `task ${chunk.id} ended: ${ended[chunk.data.end]}` })
      continue
    }
    if (chunk.type === "data-turn-start") {
      turns.push({ id: chunk.id!, inputs: [], steps: 0, chunks: [], errors: [], end: null })
      continue
    }
    const current = turns.at(-1)
    if (current === undefined) continue
    if (chunk.type === "data-step-carries") {
      for (const id of chunk.data.envelopes) {
        const at = queued.findIndex((mail) => JSON.stringify(mail.envelope) === JSON.stringify(id))
        current.inputs.push({ ...queued.splice(at, 1)[0], step: current.steps })
      }
      continue
    }
    if (chunk.type === "start-step") current.steps++
    if (chunk.type === "data-turn-end") current.end = chunk.data.reason
    else if (chunk.type === "error") current.errors.push(chunk.errorText!)
    else current.chunks.push(chunk)
  }
  const out = []
  for (const turn of turns) {
    out.push({ id: turn.id, inputs: turn.inputs, reply: await replyOf(turn.chunks), errors: turn.errors, end: turn.end })
  }
  return out
}

const dir = "sessions"
for (const file of readdirSync(dir).filter((name) => name.endsWith(".jsonl")).sort()) {
  const lines = readFileSync(join(dir, file), "utf8").split("\n").filter((line) => line.length > 0)
  const transcript = await transcriptOf(lines.map((line) => JSON.parse(line)))
  writeFileSync(join(dir, file.replace(/\.jsonl$/, ".turns.json")), JSON.stringify(transcript, null, 2) + "\n")
}

const documents = [
  JSON.stringify({ path: "src/lib.rs", old: "a\"b\\c\n", new: [1, -2.5e3, true, null, { x: "é" }], n: false }),
  JSON.stringify([{ a: [[]] }, "\u{1F600}", false, 0, "\u0001"]),
  '{"a": 12.5e-3, "b" : [ -0 , tru',
  '  {"k": nul',
  '{"x": "\\u00e9\\u12'
]
const cases: [string, string][] = []
for (const document of documents) {
  const characters = Array.from(document)
  for (let end = 0; end <= characters.length; end++) {
    const prefix = characters.slice(0, end).join("")
    cases.push([prefix, fixJson(prefix)])
  }
}
for (const prefix of ["", "{", "[", "\"abc", "\"a\\", "tr", "f", "nul", "12.", "-", "[1, 2", "[1, ", "{\"a\": ", "{\"a\"", "{\"a\": 1e", "x", "{\"a\": fals", "[t"]) {
  cases.push([prefix, fixJson(prefix)])
}
writeFileSync("fix-json.json", JSON.stringify(cases) + "\n")
