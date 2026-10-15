// @ts-check
// Current sources run against a tiny fixture by default. BASE_URL explicitly
// selects deployment monitoring. No full Wikipedia export is needed in CI.
const { defineConfig } = require('@playwright/test');

module.exports = defineConfig({
  testDir: './tests',
  timeout: 60_000,
  expect: { timeout: 15_000 },
  retries: process.env.CI ? 1 : 0,
  reporter: process.env.CI ? [['github'], ['html', { open: 'never' }]] : 'list',
  use: {
    baseURL: process.env.BASE_URL || 'http://127.0.0.1:8770',
    trace: 'retain-on-failure',
  },
  webServer: process.env.BASE_URL ? undefined : {command:'python3 ../tools/preview.py --port 8770',url:'http://127.0.0.1:8770',reuseExistingServer:!process.env.CI},
  projects: [{ name: 'chromium', use: { browserName: 'chromium' } }],
});
