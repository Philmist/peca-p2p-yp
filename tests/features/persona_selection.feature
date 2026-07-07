# 掲載前のペルソナ選択と配信中ロック(003-persona-selection)
#
# US1(選択操作)と US2(配信中ロック)の受け入れシナリオ。契約(contracts/local-api.md §1)
# の Gherkin と spec の受け入れ・edge case に対応する。直接 API を叩いて「UI のみの防御では
# ない」こと(バックエンド強制)とネガティブ(配信中バイパス試行 → 409)を検証する。
Feature: 掲載前のペルソナ選択と配信中ロック

  Background:
    Given ペルソナ選択のテスト環境を初期化する

  # --- US1: 選択操作 ------------------------------------------------------------

  Scenario: 非配信中に有効ペルソナを選択できる
    Given active かつ usable なペルソナ "A" と "B" が存在し、何も発行していない
    When ペルソナ "B" を選択する API を送る
    Then ステータス 204 が返る
    And 選択中ペルソナは "B" である

  Scenario: archived は選択できない
    Given archived なペルソナ "D" が存在する
    When ペルソナ "D" を選択する API を送る
    Then ステータス 409 とエラーコード "persona_not_selectable" が返る

  # --- US2: 配信中ロック --------------------------------------------------------

  Scenario: 配信中は selected の切替が拒否される(直接 API)
    Given ペルソナ "A" が選択中で発行中、別ペルソナ "B" が存在する
    When ペルソナ "B" を選択する API を送る
    Then ステータス 409 とエラーコード "broadcasting_locked" が返る
    And 選択中ペルソナは "A" である

  Scenario: 配信中は selected の破棄・アーカイブが拒否される
    Given ペルソナ "A" が選択中で発行中、別ペルソナ "B" が存在する
    When ペルソナ "A" をアーカイブする API を送る
    Then ステータス 409 とエラーコード "broadcasting_locked" が返る
    When ペルソナ "A" を破棄する API を送る
    Then ステータス 409 とエラーコード "broadcasting_locked" が返る

  Scenario: 配信中でも label 変更と他ペルソナ操作は許可される
    Given ペルソナ "A" が選択中で発行中、別ペルソナ "C" が存在する
    When ペルソナ "A" の label を "新名" に変更する API を送る
    Then ステータス 204 が返る
    When ペルソナ "C" を破棄する API を送る
    Then ステータス 204 が返る

  Scenario: 停止後はロックが解ける
    Given ペルソナ "A" が選択中で発行中、別ペルソナ "B" が存在する
    When すべてのチャンネルが終了する
    And ペルソナ "B" を選択する API を送る
    Then ステータス 204 が返る
    And 選択中ペルソナは "B" である

  Scenario: 古い画面状態から送信された制限操作は拒否され状態が最新化される
    Given ペルソナ "A" が選択中で発行中、別ペルソナ "B" が存在する
    When ペルソナ "B" を選択する API を送る
    Then ステータス 409 とエラーコード "broadcasting_locked" が返る
    And 選択中ペルソナは "A" である
    And GET status の broadcasting は "true" である
