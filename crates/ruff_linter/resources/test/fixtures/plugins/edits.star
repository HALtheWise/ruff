# Fixes that delete and insert code, which the edits keep valid wherever the node is: deleting a
# call statement deletes its line (or leaves `pass` in a block of its own), deleting an item of a
# list deletes its comma, and inserted lines get the statement's indentation.

DROP = rule("EDIT001", "drop-marked")
INSERT = rule("EDIT002", "insert-next-to-marked")

def drop(node):
    DROP.report(node, "`{}` is marked for deletion".format(node.text), fix = delete(node), fix_title = "Delete it")

# Calls like `remove_me()` delete their statement.
def check_call(call):
    if call.func().text == "remove_me":
        drop(call)
        return

    # Deleting adjacent keyword arguments in one fix merges the deletions of their shared comma.
    pairs = [keyword for keyword in call.keywords() if keyword.arg and keyword.arg.startswith("pair_")]
    if pairs:
        DROP.report(call, "Remove the `pair_` arguments", fix = [delete(keyword) for keyword in pairs])

nodes("Call").foreach(check_call)

# Names, imported names, and keyword arguments like `drop_arg` delete themselves from their list.
nodes("Name", "alias", "keyword").filter_name("^drop_").foreach(drop)

# Each fix also renames its marker, so that it only applies once.
def check_statement(stmt):
    marker = stmt.value().text
    done = replace(stmt.value(), "done()")
    if marker == "before()":
        INSERT.report(stmt, "Set up first", fix = [insert_before(stmt, "setup()"), done])
    elif marker == "after()":
        # Inserted lines after the first get the statement's indentation too.
        INSERT.report(stmt, "Tear down after", fix = [insert_after(stmt, "if ready:\n    teardown()"), done])
    elif marker == "inline()":
        # A statement that shares its line gets a `;`.
        INSERT.report(stmt, "Flush after", fix = [insert_after(stmt, "flush()"), done])

nodes("Expr").foreach(check_statement)
