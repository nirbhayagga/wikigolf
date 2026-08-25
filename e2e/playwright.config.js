// @ts-check
// End-to-end smoke tests run against a DEPLOYED site, not a build: the
// static tree needs the 18 GB export and the live server needs the parquet,
// neither of which CI can produce. BASE_URL picks the target; the default is
// production, which is what the daily schedule is for.
const { defineConfig } = require('@playwright/test');

module.exports = defineConfig({
  testDir: './tests',
  timeout: 60_000,
  expect: { timeout: 15_000 },
  retries: process.env.CI ? 1 : 0,
  reporter: process.env.CI ? [['github'], ['html', { open: 'never' }]] : 'list',
  use: {
    baseURL: process.env.BASE_URL || 'https://wikigolf.app',
    trace: 'retain-on-failure',
  },
  projects: [{ name: 'chromium', use: { browserName: 'chromium' } }],
});
