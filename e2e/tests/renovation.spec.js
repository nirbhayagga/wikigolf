const {test,expect}=require('@playwright/test');
// Fixture regression tests use current sources. Production monitoring lives
// in smoke.spec.js and is selected separately via BASE_URL.
test.skip(!!process.env.BASE_URL, 'local fixture suite');
test('shared random race reproduces the exact endpoints and rules',async({page,context})=>{
  await page.goto('/'); await page.locator('#new').click();
  await expect(page.locator('#veilbtn')).toHaveText('Start race');
  const start=await page.locator('#cur').innerText(), goal=await page.locator('#goal').innerText();
  const other=await context.newPage(); await other.goto(page.url());
  await expect(other.locator('#cur')).toHaveText(start); await expect(other.locator('#goal')).toHaveText(goal);
  await expect(other.locator('#notice')).toBeHidden();
});
test('paused links are inert and keyboard activation cannot advance the race',async({page})=>{
  await page.goto('/'); await page.locator('#veilbtn').click(); await page.locator('#pause').click();
  await expect(page.locator('#links')).toHaveAttribute('inert','');
  await page.locator('#links button.link:not([disabled])').first().press('Enter');
  await expect(page.locator('#cur')).toHaveText('Photography'); await expect(page.locator('#clicks')).toHaveText('0');
});
test('goal, start and controls are visible on a phone',async({page})=>{
  await page.setViewportSize({width:390,height:844}); await page.goto('/');
  await expect(page.locator('#goal')).toBeInViewport(); await expect(page.locator('#veilbtn')).toBeInViewport();
  await expect(page.locator('#pause')).toBeInViewport(); await expect(page.locator('#cv')).toBeHidden();
});
test('map failure does not prevent playing',async({page})=>{
  await page.route('**/map.json',route=>route.abort()); await page.goto('/');
  await page.locator('#explore').click();
  await expect(page.locator('#mapstatus')).toBeVisible();
  await page.locator('#closemap').click();
  await page.locator('#veilbtn').click(); await page.locator('#lfilter').fill('Light');
  await page.locator('#links button.link').click(); await expect(page.locator('#cur')).toHaveText('Light');
});
test('map can retry a failed load and does not change the player article',async({page})=>{
  await page.route('**/map.json',route=>route.abort()); await page.goto('/');
  await page.locator('#explore').click();
  await expect(page.locator('#mapstatus')).toBeVisible();
  await page.unroute('**/map.json');
  await page.locator('#mapretry').click();
  await expect(page.locator('#mapstatus')).toBeHidden();
  await expect(page.locator('#topiclist .topic-card').first()).toBeVisible();
  await page.locator('#topiclist .topic-card').first().click();
  await expect(page.locator('#cv')).toBeVisible();
  await expect(page.locator('#mapregion')).not.toHaveValue('');
  await page.locator('#zfit').click();
  await page.keyboard.press('Escape');
  await expect(page.locator('#stage')).toBeHidden();
  await expect(page.locator('#explore')).toBeFocused();
  await expect(page.locator('#cur')).toHaveText('Photography');
  await expect(page.locator('#clicks')).toHaveText('0');
});
test('three-hole round links restore the same numbered round',async({page,context})=>{
  await page.goto('/?daily=2&difficulty=medium&dataset=fixture-v1');
  await page.locator('#round').click();
  await expect(page.locator('#pnum')).toHaveText('Round #2 · hole 1/3');
  const other=await context.newPage(); await other.goto(page.url());
  await expect(other.locator('#pnum')).toHaveText('Round #2 · hole 1/3');
  await expect(other.locator('#cur')).toHaveText(await page.locator('#cur').innerText());
  await expect(other.locator('#goal')).toHaveText(await page.locator('#goal').innerText());
});
test('progress resumes at the saved article with hint budget preserved',async({page})=>{
  await page.goto('/'); await page.locator('#veilbtn').click();
  await page.locator('#compass').click();
  await expect(page.locator('#compass')).toHaveText('Hint shown');
  await page.locator('#lfilter').fill('Light'); await page.locator('#links button.link').click();
  await expect(page.locator('#cur')).toHaveText('Light'); await page.locator('#pause').click();
  await page.reload(); await page.locator('#resumeSaved').click();
  await expect(page.locator('#cur')).toHaveText('Light'); await expect(page.locator('#veiltitle')).toHaveText('Paused');
  await expect(page.locator('#clicks')).toHaveText('1');
  await page.locator('#veilbtn').click();
  await expect(page.locator('#compass')).toBeDisabled();
  await expect(page.locator('#compass')).not.toContainText('1 left');
});
test('finish reveals the shortest route and leaves the game inert',async({page})=>{
  await page.goto('/'); await page.locator('#veilbtn').click();
  for(const title of ['Light','Sun','Solar System']) {
    await page.locator('#lfilter').fill(title); await page.locator('#links button.link').click();
    await expect(page.locator('#cur')).toHaveText(title);
  }
  await expect(page.locator('#result')).toBeVisible(); await expect(page.locator('#route .step')).toHaveCount(4);
  await expect(page.locator('#back')).toBeDisabled();
});
test('daily archive shares its number and difficulty',async({page,context})=>{
  await page.goto('/?daily=2&difficulty=hard&dataset=fixture-v1');
  const other=await context.newPage(); await other.goto(page.url());
  await expect(other.locator('#pnum')).toHaveText('#2'); await expect(other.locator('#ddiff')).toHaveValue('hard');
});
test('future dailies are rejected even when pre-exported',async({page})=>{
  await page.goto('/?daily=99999&difficulty=medium&dataset=fixture-v1');
  await expect(page.locator('#notice')).toContainText('does not exist yet');
});

test('an unmeasured opening hint keeps its charge for the next article',async({page})=>{
  await page.route('**/compass/*.json',route=>route.fulfill({json:{medium:{d:2,l:[[2],[]]}}}));
  await page.goto('/'); await page.locator('#veilbtn').click();
  await page.locator('#compass').click();
  await expect(page.locator('#compass')).toHaveText('Hint saved · 1 left');
  await expect(page.locator('#compass')).toBeDisabled();
  await expect(page.locator('.compassnote')).toContainText('Your hint was saved');
  await page.locator('#lfilter').fill('Light'); await page.locator('#links button.link').click();
  await expect(page.locator('#compass')).toBeEnabled();
  await expect(page.locator('#compass')).toHaveText('Hint · 1 left');
  await page.locator('#compass').click();
  await expect(page.locator('#compass')).toHaveText('Hint shown');
  await page.locator('#lfilter').fill('Sun'); await page.locator('#links button.link').click();
  await expect(page.locator('#compass')).toBeHidden();
});

test('a distance on a blocked hub does not consume the hint',async({page})=>{
  await page.route('**/daily/*.json',async route=>{
    const response=await route.fetch(), body=await response.json();
    body.difficulties.medium.ban_degree=102;
    await route.fulfill({json:body});
  });
  await page.route('**/compass/*.json',route=>route.fulfill({json:{medium:{d:2,l:[[7],[]]}}}));
  await page.goto('/'); await page.locator('#veilbtn').click();
  await page.locator('#compass').click();
  await expect(page.locator('#compass')).toHaveText('Hint saved · 1 left');
});

test('practice avoids recent endpoints while shared links still reproduce them',async({page,context})=>{
  const articles=require('../fixtures/shards/0.json').a;
  const art=id=>({id,title:articles[id][1],desc:articles[id][2],in_degree:articles[id][9]});
  const races=[0,1,2].map(id=>({start:art(id),goal:art(id+3),par:3,routes:1,
    route:[id,id+1,id+2,id+3].map(i=>[i,articles[i][1],null,null])}));
  await context.route('**/random/medium.json',route=>route.fulfill({json:{ban_degree:null,compass:false,races}}));
  await page.goto('/'); await page.locator('#new').click();
  await expect(page.locator('#pnum')).toHaveText('');
  await expect(page.locator('#new')).toBeEnabled();
  const first=[await page.locator('#cur').innerText(),await page.locator('#goal').innerText()];
  const shared=page.url();
  await page.locator('#new').click();
  await expect(page.locator('#cur')).not.toHaveText(first[0]);
  await expect(page.locator('#goal')).not.toHaveText(first[1]);
  const other=await context.newPage(); await other.goto(shared);
  await expect(other.locator('#cur')).toHaveText(first[0]);
  await expect(other.locator('#goal')).toHaveText(first[1]);
});
