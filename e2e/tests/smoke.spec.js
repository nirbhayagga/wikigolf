// @ts-check
// The player's path through one race, against the real deployed site. Each
// test is a regression that actually happened in playtesting: the give-up
// flow once wiped the route box and printed "could not check", and a lagged
// double-click once counted twice.
const { test, expect } = require('@playwright/test');

test.beforeEach(async ({ page }) => {
  // The first-visit tutorial would cover the board.
  await page.addInitScript(() => {
    try { localStorage.setItem('wr-help', 'off'); } catch {}
  });
});

// A link that is not the goal, so a click never accidentally finishes.
async function safeLink(page) {
  const goal = (await page.locator('#goal').textContent()) || '';
  const links = page.locator('#links button.link:not([disabled])');
  await expect(links.first()).toBeVisible();
  return links.filter({ hasNotText: goal.trim() }).first();
}

test("loads and arms today's daily", async ({ page }) => {
  await page.goto('/');
  await expect(page).toHaveTitle(/WikiGolf/i);
  await expect(page.locator('#veilbtn')).toHaveText('Start race');
  await expect(page.locator('#goal')).not.toHaveText('—');
  await expect(page.locator('#goalmeta')).toContainText('inbound links');
});

test('start, click, give up: the route reveal survives', async ({ page }) => {
  await page.goto('/');
  await page.locator('#veilbtn').click();
  await (await safeLink(page)).click();
  await expect(page.locator('#clicks')).toHaveText('1');
  await page.locator('#giveup').click();
  await expect(page.locator('#result')).toBeVisible();
  await expect(page.locator('#verdict')).toContainText('Gave up');
  await expect(page.locator('#routetxt')).toContainText('A shortest route');
  await expect(page.locator('#route .step').first()).toBeVisible();
  await expect(page.locator('#result')).not.toContainText('could not check');
});

test('a lagged double-click counts once', async ({ page }) => {
  await page.goto('/');
  await page.locator('#veilbtn').click();
  await (await safeLink(page)).dblclick();
  await expect(page.locator('#clicks')).toHaveText('1');
  // and the trail names the article once, not twice
  const trail = await page.locator('#trail').textContent();
  const names = (trail || '').split('›').map(s => s.trim());
  expect(new Set(names).size).toBe(names.length);
});

test('a random race arms with a real goal', async ({ page }) => {
  await page.goto('/');
  await page.locator('#new').click();
  await expect(page.locator('#veilbtn')).toHaveText('Start race');
  await expect(page.locator('#goalmeta')).toContainText('inbound links');
  await expect(page.locator('#goalmeta')).not.toContainText('0 inbound links');
});
