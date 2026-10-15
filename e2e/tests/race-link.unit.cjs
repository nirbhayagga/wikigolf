const {test}=require('node:test');
const assert=require('node:assert/strict');
const link=require('../../static/race-link.js');
const meta={dataset:'content-a'};
test('shared pairs preserve titles, difficulty, dataset and hub rules',()=>{
  const p={start:{title:'AT&T'},goal:{title:'İstanbul / A+B'},difficulty:'hard',ban_degree:42};
  const q=link.decode(link.encode('https://example.test/?old=1',p,meta),meta);
  assert.deepEqual(q,{mode:'pair',from:'AT&T',to:'İstanbul / A+B',difficulty:'hard',ban:'42'});
});
test('daily and round archives reproduce mode and number',()=>{
  assert.deepEqual(link.decode(link.encode('https://example.test/',{number:17,difficulty:'easy'},meta),meta),{mode:'daily',number:17,difficulty:'easy'});
  assert.deepEqual(link.decode(link.encode('https://example.test/',{},meta,{number:18,holes:[1,2,3]}),meta),{mode:'round',number:18,holes:3});
});
test('foreign datasets, malformed pairs and numbers fail closed',()=>{
  for(const query of ['dataset=content-b','from=Cat','daily=-1','daily=NaN','round=1&holes=5','from=A&to=B&ban=NaN'])
    assert.throws(()=>link.decode('https://example.test/?'+query,meta));
});
test('legacy title links stay recognizable without silently becoming random',()=>{
  assert.equal(link.decode('https://example.test/?from=A&to=B',meta).mode,'pair');
});
