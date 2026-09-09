"""Isolated SQLite helper comparison, not end-to-end/semantic retrieval latency.
Uses the old per-term LIKE scan and current FTS-prefix join + population count.
The synthetic corpus makes their document counts equal so work is comparable.
"""
from pathlib import Path
import argparse,json,platform,sqlite3,statistics,time
TERMS=['narwhal','quokka','walrus','puffin','ibex','ocelot','tapir','wombat']
OLD_N='SELECT COUNT(*) FROM memories WHERE invalidated_at IS NULL'
OLD_DF='SELECT COUNT(*) FROM memories WHERE invalidated_at IS NULL AND lower(text) LIKE ?'
NEW_N='SELECT COUNT(*) FROM memories_fts JOIN memories m USING(memory_id) WHERE m.invalidated_at IS NULL'
NEW_DF='SELECT COUNT(DISTINCT m.memory_id) FROM memories_fts JOIN memories m USING(memory_id) WHERE memories_fts MATCH ? AND m.invalidated_at IS NULL'
def measure(conn,new):
 n=conn.execute(NEW_N if new else OLD_N).fetchone()[0]
 counts=[conn.execute(NEW_DF if new else OLD_DF, ('text : "'+term+'"*' if new else '%'+term+'%',)).fetchone()[0] for term in TERMS]
 return n,counts

def main():
 parser=argparse.ArgumentParser(description=__doc__);parser.add_argument('--sizes',default='1000,10000,100000,1000000');parser.add_argument('--repeats',type=int,default=5);parser.add_argument('--out',type=Path,required=True);args=parser.parse_args()
 result={'scope':'isolated SQL helper, synthetic aligned token semantics, in-memory SQLite; excludes ANN/model/reranking/serialization','sqlite':sqlite3.sqlite_version,'platform':platform.platform(),'processor':platform.processor(),'repeats':args.repeats,'rows':[]}
 for n in map(int,args.sizes.split(',')):
  c=sqlite3.connect(':memory:');c.executescript('CREATE TABLE memories(memory_id TEXT PRIMARY KEY,text TEXT,invalidated_at TEXT);CREATE VIRTUAL TABLE memories_fts USING fts5(memory_id UNINDEXED,text);')
  def corpus():
   for i in range(n):
    term=TERMS[i%808] if i%808<len(TERMS) else 'routine'
    yield str(i), f'Technical note {i}: {term} module has repeatable configuration instructions and local verification details. The fixture contains neutral background text for a controlled SQL document frequency workload.'
  c.executemany('INSERT INTO memories(memory_id,text) VALUES(?,?)',corpus());c.execute('INSERT INTO memories_fts(memory_id,text) SELECT memory_id,text FROM memories');c.commit()
  expected=measure(c,False);assert measure(c,True)==expected
  times={'old_like':[],'indexed_fts':[]}
  for repeat in range(args.repeats):
   for new in ([False,True] if repeat%2==0 else [True,False]):
    start=time.perf_counter();actual=measure(c,new);elapsed=(time.perf_counter()-start)*1000;assert actual==expected
    times['indexed_fts' if new else 'old_like'].append(elapsed)
  row={'memories':n,'term_document_counts':expected[1],'samples_ms':times,'old_median_ms':statistics.median(times['old_like']),'new_median_ms':statistics.median(times['indexed_fts'])}
  row['ratio_old_over_new']=row['old_median_ms']/row['new_median_ms'];result['rows'].append(row)
  args.out.write_text(json.dumps(result,indent=2),encoding='utf-8');print(json.dumps(row),flush=True);c.close()
if __name__=='__main__':main()
