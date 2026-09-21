#!/usr/bin/env python3
import json
import urllib.request
import sys

API = 'http://localhost:9088/api/v1'

def api_get(path):
    with urllib.request.urlopen(f'{API}{path}', timeout=10) as resp:
        return json.loads(resp.read().decode('utf-8'))

def api_put(path, data):
    body = json.dumps(data).encode('utf-8')
    req = urllib.request.Request(f'{API}{path}', data=body, headers={'Content-Type': 'application/json'}, method='PUT')
    with urllib.request.urlopen(req, timeout=10) as resp:
        return json.loads(resp.read().decode('utf-8'))

def main():
    cfg = api_get('/config')

    W = 3270.0
    H = 1960.0
    aspect = W / H

    warp_corners = {
        'DP-5': [[0.020, 0.015], [0.985, 0.010], [0.990, 0.980], [0.015, 0.985]],
        'DP-6': [[0.015, 0.010], [0.980, 0.020], [0.985, 0.985], [0.010, 0.980]],
        'DP-7': [[0.015, 0.020], [0.990, 0.015], [0.980, 0.980], [0.020, 0.985]],
        'DP-8': [[0.010, 0.015], [0.985, 0.020], [0.975, 0.985], [0.015, 0.980]],
    }

    output_boxes = {
        'DP-5': (0.0, 0.0, 1920.0, 1200.0),
        'DP-6': (1350.0, 0.0, 1920.0, 1200.0),
        'DP-7': (0.0, 760.0, 1920.0, 1200.0),
        'DP-8': (1330.0, 760.0, 1920.0, 1200.0),
    }

    cfg['projection']['mode'] = 'warp'
    cfg['projection']['canvas'] = {
        'aspect': aspect,
        'renderWidth': 3270,
        'scale': 1.0
    }

    for out in cfg['outputs']:
        name = out['match']['name']
        if name in output_boxes:
            bx, by, bw, bh = output_boxes[name]
            src = {
                'x': bx / W,
                'y': by / W,
                'width': bw / W,
                'height': bh / W
            }
            out['mode'] = {
                'width': 1920,
                'height': 1200,
                'refreshHz': 59.95
            }
            out['geometry'] = {
                'source': src,
                'corners': warp_corners[name],
                'center': [0.5, 0.5],
                'rasterFootprint': src
            }

    if 'revision' in cfg:
        del cfg['revision']
    cfg['committed'] = True

    res = api_put('/config', cfg)
    print('PUT /config status:', res.get('committed'), 'revision:', res.get('revision'))

if __name__ == '__main__':
    main()
