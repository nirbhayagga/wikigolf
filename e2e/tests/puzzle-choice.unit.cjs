const {test}=require('node:test');
const assert=require('node:assert/strict');
const P=require('../../static/puzzle-choice.js');
const row=(a,b)=>({start:{id:a},goal:{id:b}});
test('prefers fresh endpoints and never discards a fresh pair for an old one',()=>{
  const a=row(1,2),b=row(1,3),c=row(4,5);
  assert.equal(P.choose([a,b,c],[],[1,2]),c);
  assert.equal(P.choose([a,b,c],['4-5'],[1,2]),b);
});
test('duplicate export rows do not give a pair extra selection weight',()=>{
  const a=row(1,2),b=row(3,4);
  assert.equal(P.choose([a,a,a,b],[],[],()=>0.6),b);
});
test('small or exhausted pools keep working with minimum endpoint repetition',()=>{
  const a=row(1,2),b=row(1,3);
  assert.equal(P.choose([a,b],['1-2','1-3'],[1,2,2]),b);
  assert.equal(P.choose([a],['1-2'],[1,2]),a);
  assert.throws(()=>P.choose([]),/No puzzles/);
});
test('legacy tuple pools keep the same endpoint identity',()=>{
  const a=[1,'one',2,'two',3,4];
  assert.equal(P.key(a),'1-2');
  assert.equal(P.choose([a],[],[]),a);
});
