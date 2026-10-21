import unittest
import importlib.util
import tempfile
import json
from pathlib import Path

PATH=Path(__file__).with_name('semantic-memory-live-quality.py')
spec=importlib.util.spec_from_file_location('quality_evaluation',PATH)
module=importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)

class QualityContractTests(unittest.TestCase):
    def test_metrics_count_fp_fn_as_well_as_tp(self):
        self.assertEqual(module.metrics({'tp':2,'fp':1,'fn':2})['recall'],0.5)
        self.assertAlmostEqual(module.metrics({'tp':2,'fp':1,'fn':2})['precision'],2/3)
        self.assertEqual(module.metrics({'tp':0,'fp':0,'fn':5})['f1'],0.0)

    def test_fails_closed_without_real_gold_labels(self):
        with tempfile.TemporaryDirectory() as folder:
            file=Path(folder)/'gold.json'
            file.write_text(json.dumps({'cases':[]}))
            with self.assertRaisesRegex(ValueError,'human'):
                module.load_gold(file)

    def test_refuses_unapproved_remote_fact_disclosure(self):
        facts=[{'id':'a','text':'private context'}]
        with self.assertRaisesRegex(ValueError,'explicit consent'):
            module.call_model('https://models.example.com/v1/chat/completions',
                'model','MISSING_KEY',facts,30,False)
        
    def test_agreement_requires_matching_meaning_not_only_ids(self):
        facts=[{'id':'a','text':'brief reports'},
               {'id':'b','text':'concise summaries'}]
        first=module.proposal_set([{'kind':'merge','ids':['a','b'],
                                   'suggestedText':'Prefers concise summaries'}],facts)
        second=module.proposal_set([{'kind':'merge','ids':['b','a'],
                                    'suggestedText':'Prefers very long reports'}],facts)
        self.assertNotEqual(first[('merge',('a','b'))],
                            second[('merge',('a','b'))])

if __name__=='__main__':
    unittest.main()
