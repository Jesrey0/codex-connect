import { build } from "esbuild";
import { readFile, writeFile } from "node:fs/promises";
import { createHash } from "node:crypto";

const result = await build({
  entryPoints: ["src/main.ts"], bundle: true, write: false,
  format: "iife", target: "es2022", minify: true, legalComments: "inline",
  // Emit escaped strings so SDK template whitespace does not dirty the embedded asset.
  supported: { "template-literal": false },
  metafile: true,
});
const packages = [...new Set(Object.keys(result.metafile.inputs)
  .filter(path => path.includes("node_modules/"))
  .map(path => {
    const dependency = path.split("node_modules/")[1];
    return dependency.split("/").slice(0, dependency.startsWith("@") ? 2 : 1).join("/");
  }))].sort();
const notices = (await Promise.all(packages.map(async name =>
  `${name}\n${await readFile(`node_modules/${name}/LICENSE`, "utf8")}`))).join("\n\n").replaceAll("-->", "-- >");
const script = result.outputFiles[0].text.replace(/<\/script/gi, "<\\/script");
const style = await readFile("src/style.css", "utf8");
const hash = text => createHash("sha256").update(text).digest("base64");
const policy = `default-src 'none'; script-src 'sha256-${hash(script)}'; style-src 'sha256-${hash(style)}'; connect-src 'none'; img-src 'none'; frame-src 'none'; object-src 'none'; base-uri 'none'; form-action 'none'`;
const html = `<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><meta http-equiv="Content-Security-Policy" content="${policy}"><title>Workers</title><!-- Bundled dependency licenses\n${notices}\n--><style>${style}</style></head><body><div id="root"><p role="status">Connecting to the worker panel…</p></div><script>${script}</script></body></html>\n`;
if (process.argv.includes("--check")) {
  if (await readFile("workers.html", "utf8") !== html) {
    throw new Error("Embedded Workers HTML is stale. Run npm run build.");
  }
} else {
  await writeFile("workers.html", html);
  console.log(`Embedded Workers HTML: ${Buffer.byteLength(html)} bytes`);
}
