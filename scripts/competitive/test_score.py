"""Adversarial scoring checks: false wins must stay unpublishable."""
import copy
import hashlib
import json
import math
import unittest
from unittest.mock import patch

import cases
import score

CATALOG = [c for c in cases.CASES if c['id'] in ('sum','int64_exact','range_page','all_rows_wide')]
ENGINES = ['sqlite','sqlite-aa','duckdb','powdb-powql','powdb-sql']


def evidence():
    records=[]
    for engine in ENGINES:
        for round_number in range(5):
            for case in CATALOG:
                preview,rows = {'sum':([[-500]],1), 'int64_exact':([[9007199254740993]],1),
                                'range_page':([[1,-499]],1),
                                'all_rows_wide':([[i,f'user_{i}','x'*128] for i in range(3)],1000)}[case['id']]
                for delivery in case['deliveries']:
                    sample=dict(elapsed_ns=1000,first_row_ns=500,rows=rows,preview=preview,digest='a'*64)
                    records.append(dict(engine=engine,round=round_number,case=case['id'],delivery=delivery,
                        queries=['query']*case['iterations'],samples=[copy.deepcopy(sample) for _ in range(case['iterations'])],
                        producer='fixture',memory=dict(peak_pss_kib=1000,samples=5,errors=[])))
    return dict(schema=1,profile=cases.PROFILE,rows=1000,rounds=5,
        cases_sha256=hashlib.sha256(json.dumps(CATALOG,sort_keys=True).encode()).hexdigest(),
        selected_cases=[c['id'] for c in CATALOG],isolated_host_assertion=True,failures=[],records=records,
        environment=dict(git_dirty=False,git_sha='fixture',build_manifest={'source_sha':'fixture','artifacts_sha256':{'lib.so':'c'*64,'bin/sqlite':'a'*64,'bin/duckdb':'b'*64,'bin/powdb':'b'*64}},
            binaries={e:{'sha256':('a' if e.startswith('sqlite') else 'b')*64} for e in ENGINES}))


class Scoring(unittest.TestCase):
    def setUp(self):
        self.catalog_patch=patch.object(score,'CASES',CATALOG)
        self.catalog_patch.start()
        self.addCleanup(self.catalog_patch.stop)
        self.data=evidence()

    def scored(self):
        return score.score(self.data)

    def assert_withheld(self):
        self.assertTrue(all(v is None for v in self.scored()['cohorts']['embedded_native']['common_query_latency_score'].values()))

    def test_qualified_control(self):
        self.assertEqual(self.scored()['cohorts']['embedded_native']['common_query_latency_score'],
                         {'sqlite':100.0,'duckdb':100.0,'powdb-powql':100.0})

    def test_wrong_answer_cannot_win(self):
        for r in self.data['records']:
            if r['engine']=='powdb-powql':
                r['samples'][0]['digest']='f'*64
        result=self.scored()['cohorts']['embedded_native']
        self.assertIsNone(result['common_query_latency_score']['powdb-powql'])
        self.assertIn('powdb-powql',result['correctness_failed'])

    def test_shared_loader_error_rejected(self):
        for r in self.data['records']:
            if r['case']=='int64_exact':
                r['samples'][0]['preview']=[[9007199254740992]]
        with self.assertRaisesRegex(ValueError,'fixture invariant'):
            self.scored()

    def test_noisy_identical_binary_withholds_rank(self):
        for r in self.data['records']:
            if r['engine']=='sqlite-aa':
                for sample in r['samples']:sample['elapsed_ns']=1200
        self.assert_withheld()

    def test_missing_round_withholds_rank(self):
        self.data['records'].pop(0)
        self.assert_withheld()

    def test_dirty_shared_partial_runs_withheld(self):
        self.data['environment']['git_dirty']=True
        self.data['isolated_host_assertion']=False
        self.data['selected_cases'].pop()
        self.assert_withheld()

    def test_nan_zero_and_infinite_timings_rejected(self):
        for value in (0,-1,math.nan,math.inf):
            self.data=evidence()
            self.data['records'][0]['samples'][0]['elapsed_ns']=value
            self.assert_withheld()

    def test_unknown_memory_stays_unknown(self):
        self.data['records'][0]['memory']['peak_pss_kib']=None
        cell=next(c for c in self.scored()['cells'] if c['engine']=='sqlite' and c['case']==self.data['records'][0]['case'])
        self.assertIsNone(cell['sampled_peak_pss_kib'])

    def test_duplicate_observation_rejected(self):
        self.data['records'].append(self.data['records'][0])
        with self.assertRaisesRegex(ValueError,'duplicate'):self.scored()

    def test_invalid_round_bound_rejected(self):
        self.data['rounds']=100000000
        with self.assertRaisesRegex(ValueError,'round'):self.scored()

    def test_small_samples_have_no_tail(self):
        self.assertIsNone(score.quantile([1,2,3],.99))
        self.assertEqual(score.quantile(list(range(1000)),.99),989)

    def test_sql_unsupported_remains_coverage_gap(self):
        for r in self.data['records']:
            if r['engine']=='powdb-sql' and r['case']=='int64_exact':
                r['error']='unsupported: synthetic capability probe'
        result=self.scored()['cohorts']['embedded_sql']
        self.assertEqual(result['query_coverage']['powdb-sql'],{'correct':4,'required':5})
        self.assertTrue(any(c['case']=='int64_exact' for c in result['excluded_cases']))


class Workloads(unittest.TestCase):
    def test_points_visit_diverse_keys_even_adversarial_fixture_size(self):
        for rows in (1000,10000,104729,1000000):
            params=cases.parameters(rows,0)
            keys=[cases.id_for(i,params) for i in range(1000)]
            self.assertEqual(len(set(keys)),1000)

    def test_keyset_and_offset_describe_same_page(self):
        for rows in (1000,104729):
            params=cases.parameters(rows,0)
            ordered=sorted(((i*37)%1000,i) for i in range(rows))
            keyset=[x for x in ordered if x>(params['key'],params['last'])][:100]
            self.assertEqual(keyset,ordered[params['offset']:params['offset']+100])


if __name__=='__main__':unittest.main()
