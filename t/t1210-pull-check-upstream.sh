#!/bin/sh

test_description='Check that pulling works when no upstream is configured'

. ./test-lib.sh

# Need a repo to clone
test_create_repo upstream

test_expect_success \
    'Setup upstream repo, clone it, and add patches to the clone' \
    '
    (cd upstream && stg init) &&
    stg clone upstream clone
    (cd clone && git config pull.rebase false)
    '

test_expect_success \
    'Test that pull works' \
    '
    (cd clone &&
      git checkout master &&
      stg pull
    )
    '

test_expect_success \
    'Test that pull without upstream setup produces friendly error' \
    '
    (cd clone &&
      stg branch --create without-upstream &&
      ( stg pull 2>&1 | grep "There is no tracking information for the current branch." ) || ( stg pull 2>&1 && false )
    )
    '

test_done
