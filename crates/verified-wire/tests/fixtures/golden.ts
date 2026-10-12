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

const transcriptOf = async (chunks: Chunk[]) => {
  const turns: { id: string; text: string; chunks: Chunk[]; errors: string[]; end: string | null }[] = []
  for (const chunk of chunks) {
    if (chunk.type === "data-user-message") {
      turns.push({ id: chunk.id!, text: chunk.data.text, chunks: [], errors: [], end: null })
      continue
    }
    const current = turns.at(-1)
    if (current === undefined || chunk.type === "data-turn-start") continue
    if (chunk.type === "data-turn-end") current.end = chunk.data.reason
    else if (chunk.type === "error") current.errors.push(chunk.errorText!)
    else current.chunks.push(chunk)
  }
  const out = []
  for (const turn of turns) {
    out.push({ id: turn.id, text: turn.text, reply: await replyOf(turn.chunks), errors: turn.errors, end: turn.end })
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
