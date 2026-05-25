const path = require("node:path");
const { pathToFileURL } = require("node:url");

const { chromium } = require("playwright");

const pages = [
  "index.html",
  "commands.html",
  "adapters.html",
  "concepts.html",
  "security.html",
  "migration.html",
];

const viewports = [
  { width: 390, height: 844, name: "mobile" },
  { width: 1440, height: 1000, name: "desktop" },
];

function pageUrl(fileName) {
  return pathToFileURL(path.resolve(__dirname, fileName)).href;
}

(async () => {
  const browser = await chromium.launch({ args: ["--no-sandbox"] });
  try {
    for (const viewport of viewports) {
      const page = await browser.newPage({
        viewport: { width: viewport.width, height: viewport.height },
      });
      for (const fileName of pages) {
        await page.goto(pageUrl(fileName), { waitUntil: "load" });
        const diagnostics = await page.evaluate(() => {
          const viewportWidth = document.documentElement.clientWidth;
          const overflowing = Array.from(document.querySelectorAll("body *"))
            .filter((element) => !element.closest("pre"))
            .map((element) => {
              const rect = element.getBoundingClientRect();
              return {
                selector:
                  element.tagName.toLowerCase() +
                  (element.className ? `.${String(element.className).split(" ").join(".")}` : ""),
                left: rect.left,
                right: rect.right,
                width: rect.width,
              };
            })
            .filter((entry) => entry.width > 0 && (entry.left < -1 || entry.right > viewportWidth + 1))
            .slice(0, 5);

          return {
            bodyText: document.body.innerText,
            documentWidth: document.documentElement.scrollWidth,
            viewportWidth,
            overflowing,
          };
        });

        if (!diagnostics.bodyText.includes("AgentMesh")) {
          throw new Error(`${fileName} rendered without AgentMesh text`);
        }
        if (diagnostics.documentWidth > diagnostics.viewportWidth + 1) {
          throw new Error(
            `${fileName} overflows ${viewport.name}: ${diagnostics.documentWidth}px > ${diagnostics.viewportWidth}px`,
          );
        }
        if (diagnostics.overflowing.length > 0) {
          throw new Error(
            `${fileName} has elements outside ${viewport.name} viewport: ${JSON.stringify(
              diagnostics.overflowing,
            )}`,
          );
        }

        const h1 = await page.locator("h1").first().boundingBox();
        if (!h1 || h1.width < 40 || h1.height < 20) {
          throw new Error(`${fileName} did not render a visible h1 on ${viewport.name}`);
        }
        const screenshot = await page.screenshot({ fullPage: false });
        if (screenshot.length < 5000) {
          throw new Error(`${fileName} produced an unexpectedly small ${viewport.name} screenshot`);
        }
      }
      await page.close();
    }
  } finally {
    await browser.close();
  }
})().catch((error) => {
  console.error(error);
  process.exit(1);
});
