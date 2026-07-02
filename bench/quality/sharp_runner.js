// Batch runner: reads a JSON jobs file, runs all sharp jobs in one process.
// Job: { mode: "resize"|"encode", input, out, maxW?, maxH?, quality, mozjpeg }
// "resize" = fit-inside resize + JPEG encode (end-to-end candidate)
// "encode" = JPEG encode only, no resize (encoder-isolation candidate)
const sharp = require("sharp");
const fs = require("fs");

sharp.concurrency(1); // per-job single thread; parallelism happens across jobs

async function run(job) {
  let img = sharp(job.input);
  if (job.mode === "resize") {
    img = img.resize(job.maxW, job.maxH, { fit: "inside", withoutEnlargement: true });
  }
  const buf = await img
    .jpeg({ quality: job.quality, mozjpeg: !!job.mozjpeg })
    .toBuffer();
  fs.writeFileSync(job.out, buf);
}

(async () => {
  const jobs = JSON.parse(fs.readFileSync(process.argv[2], "utf8"));
  const POOL = 8;
  let i = 0;
  await Promise.all(
    Array.from({ length: POOL }, async () => {
      while (i < jobs.length) {
        const job = jobs[i++];
        try {
          await run(job);
        } catch (e) {
          console.error("FAIL", job.out, e.message);
          process.exitCode = 1;
        }
      }
    })
  );
})();
